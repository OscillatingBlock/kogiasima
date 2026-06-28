use std::fs;
use std::net::Ipv4Addr;
use std::num::NonZero;
use std::path::Path;
use std::str::FromStr;

use nftables::types::NfChainType;

use futures_util::stream::TryStreamExt;

use rtnetlink::{Handle, LinkBridge, LinkUnspec, LinkVeth, RouteMessageBuilder};

use ipnetwork::IpNetwork;

use nftables::expr::{self, Expression, Meta, MetaKey, NamedExpression};
use nftables::stmt::{Match, Operator, Statement};
use nftables::{batch::Batch, helper, schema, types};

use anyhow::Context;
use uuid::Uuid;
const VETH1_HOST: &str = "veth1_host";
const VETH1_CONT: &str = "veth1_cont";

pub async fn init_network_isolation(child_pid: u32) -> anyhow::Result<()> {
    let (connection, handle, _) = rtnetlink::new_connection()?;
    tokio::spawn(connection);

    create_assign_host_bridge(&handle)
        .await
        .context("Failed to create host bridge")?;
    create_and_setup_veth(&handle, child_pid)
        .await
        .context("Failed to create and setup veth cables")?;

    Ok(())
}

async fn create_assign_host_bridge(handle: &Handle) -> anyhow::Result<()> {
    match handle
        .link()
        .add(LinkBridge::new("br0").build())
        .execute()
        .await
    {
        Ok(_) => {
            println!("[host network] br0 bridge created successfully.");
            // Only assign the IP address if we created the bridge fresh
            let ip = IpNetwork::from_str("10.0.0.1/24")?;
            add_address("br0", ip, &handle).await?;
        }
        Err(rtnetlink::Error::NetlinkError(err_msg)) if err_msg.code == NonZero::new(-17) => {
            // -17 is the Netlink error code for EEXIST (File Exists)
            println!("[host network] br0 bridge already exists, skipping creation.");
        }
        Err(other) => {
            return Err(anyhow::Error::new(other))
                .context("Failed to create network bridge instance br0");
        }
    }

    Ok(())
}

async fn add_address(
    link_name: &str,
    ip: IpNetwork,
    handle: &rtnetlink::Handle,
) -> anyhow::Result<()> {
    let mut links = handle
        .link()
        .get()
        .match_name(link_name.to_string())
        .execute();

    if let Some(link) = links.try_next().await? {
        //equal to `ip addr add 10.0.0.1/24 dev br0`
        handle
            .address()
            .add(link.header.index, ip.ip(), ip.prefix())
            .execute()
            .await
            .context("Failed to set ip address for host bridge")?;
    }

    Ok(())
}

async fn create_and_setup_veth(handle: &Handle, child_pid: u32) -> anyhow::Result<()> {
    //equal to `ip link add veth1-host type veth peer name veth1-peer`
    // handle
    //     .link()
    //     .add(LinkVeth::new(VETH1_HOST, VETH1_CONT).build())
    //     .execute()
    //     .await
    //     .context("Failed to create veth cables")?;
    //
    match handle
        .link()
        .add(LinkVeth::new(VETH1_HOST, VETH1_CONT).build())
        .execute()
        .await
    {
        Ok(_) => {
            println!("[host network] Veth cables created fresh.");
        }
        Err(rtnetlink::Error::NetlinkError(err_msg)) if err_msg.code == NonZero::new(-17) => {
            println!("[host network] Leftover orphan veth detected. Cleaning up and recreating...");

            let mut veth1_links = handle
                .link()
                .get()
                .match_name(VETH1_HOST.to_string())
                .execute();
            let veth1_link = veth1_links
                .try_next()
                .await
                .context("Failed to read from network link steam")?;

            let veth1_host_index = veth1_link
                .context("Failed to find network bridge instance br0")?
                .header
                .index;

            //  Delete the stale host-side interface
            let _ = handle.link().del(veth1_host_index).execute().await;

            //  Recreate the pair fresh so both ends exist perfectly
            handle
                .link()
                .add(LinkVeth::new(VETH1_HOST, VETH1_CONT).build())
                .execute()
                .await
                .context("Failed to recreate veth cables after cleanup")?;
        }
        Err(other) => {
            return Err(anyhow::Error::new(other)).context("Failed to create veth cables");
        }
    }

    let mut bridge_links = handle.link().get().match_name("br0".to_string()).execute();
    let bridge_index = bridge_links
        .try_next()
        .await
        .context("Failed to read from network link steam")?;

    let bridge_index = bridge_index
        .context("Failed to find network bridge instance br0")?
        .header
        .index;

    // equal to `ip link set veth1-host master br0`
    handle
        .link()
        .set(
            LinkUnspec::new_with_name(VETH1_HOST)
                .controller(bridge_index)
                .up()
                .build(),
        )
        .execute()
        .await
        .context("Failed to set veth1_host's master br0")?;

    // equal to `ip link set veth1-cont netns <child_pid>`
    handle
        .link()
        .set(
            LinkUnspec::new_with_name(VETH1_CONT)
                .setns_by_pid(child_pid)
                .up()
                .build(),
        )
        .execute()
        .await
        .context("Failed to set namespace for veth1_cont")?;

    Ok(())
}

pub async fn config_container_network() -> anyhow::Result<()> {
    let (connection, handle, _) = rtnetlink::new_connection()?;
    tokio::spawn(connection);

    //1. set loopback device up
    handle
        .link()
        .set(LinkUnspec::new_with_name("lo").up().build())
        .execute()
        .await?;

    //2. assign ip address to the container side of veth cable
    let ip = IpNetwork::from_str("10.0.0.2/24")?;
    add_address(VETH1_CONT, ip, &handle).await?;

    //3. bring veth1-cont up
    handle
        .link()
        .set(LinkUnspec::new_with_name(VETH1_CONT).up().build())
        .execute()
        .await?;

    //4.set default ip address as bridge ip for veth1-cont
    let mut veth1_cont_links = handle
        .link()
        .get()
        .match_name(VETH1_CONT.to_string())
        .execute();

    let veth1_cont_link = veth1_cont_links
        .try_next()
        .await
        .context("Failed to read from veth1-cont stream")?;
    let veth1_cont_index = veth1_cont_link
        .context("Failed to find veth1-cont")?
        .header
        .index;

    let gateway_ip = Ipv4Addr::new(10, 0, 0, 1).into();
    let route = RouteMessageBuilder::<Ipv4Addr>::new()
        .destination_prefix(Ipv4Addr::UNSPECIFIED, 0)
        .gateway(gateway_ip)
        .output_interface(veth1_cont_index)
        .build();

    handle
        .route()
        .add(route)
        .execute()
        .await
        .context("Failed to set default ip address for veth1-cont")?;

    Ok(())
}

pub fn enable_host_ip_forwarding() -> anyhow::Result<()> {
    let path = Path::new("/proc/sys/net/ipv4/ip_forward");

    println!("[host network] Ensuring kernel IPv4 forwarding is active...");

    let current_state =
        fs::read_to_string(path).context("Failed to read from /proc/sys/net/ipv4/ip_forward")?;
    if current_state.trim() != "1" {
        println!("[host network] Toggling net.ipv4.ip_forward to 1");
        fs::write(path, "1").context("Failed to write to /proc/sys/net/ipv4/ip_forward")?;
    } else {
        println!("[host network] Kernel IPv4 forwarding is already enabled.");
    }

    let current_state = fs::read_to_string(path)
        .context("Failed to read from /proc/sys/net/ipv6/conf/all/forwarding")?;
    if current_state.trim() != "1" {
        println!("[host network] Toggling net.ipv6.conf.all.forwarding to 1");
        fs::write(path, "1")
            .context("Failed to write to /proc/sys/net/ipv6/conf/all/forwarding")?;
    } else {
        println!("[host network] Kernel IPv6 forwarding is already enabled.");
    }

    Ok(())
}

pub fn setup_nftables(id: &Uuid) -> anyhow::Result<()> {
    //batch is used to prepare nftable payloads
    let mut batch = Batch::new();
    let table_name = format!("mini-docker-{}", id.as_hyphenated());
    let host_wan: &'static str = "wlan0";
    let container_ip: &'static str = "10.0.0.1/24";

    //create a new table
    batch.add(schema::NfListObject::Table(schema::Table {
        family: types::NfFamily::INet,
        name: table_name.clone().into(),
        ..Default::default()
    }));

    //create forward chain
    batch.add(schema::NfListObject::Chain(schema::Chain {
        family: types::NfFamily::INet,
        table: table_name.clone().into(),
        name: "forward_chain".into(),
        _type: Some(NfChainType::Filter),
        hook: Some(types::NfHook::Forward),
        prio: Some(0),
        policy: Some(types::NfChainPolicy::Drop),
        ..Default::default()
    }));

    //create NAT chain
    batch.add(schema::NfListObject::Chain(schema::Chain {
        family: types::NfFamily::INet,
        table: table_name.clone().into(),
        name: "nat_chain".into(),
        _type: Some(NfChainType::NAT),
        hook: Some(types::NfHook::Postrouting),
        prio: Some(100),
        policy: Some(types::NfChainPolicy::Accept),
        ..Default::default()
    }));

    add_batch_rules(
        &mut batch,
        table_name.clone().to_string(),
        host_wan.to_string(),
        container_ip.to_string(),
    );

    apply_firewall_rules(batch).context("Failed to apply firewall rules")?;
    Ok(())
}

fn add_batch_rules(batch: &mut Batch, table_name: String, host_wan: String, _container_ip: String) {
    //add rule to Translate outbound traffic, Masquerading
    batch.add(schema::NfListObject::Rule(schema::Rule {
        family: types::NfFamily::INet,
        table: table_name.clone().into(),
        chain: "nat_chain".into(),
        expr: vec![
            // A. Match output interface name == $HOST_WAN
            Statement::Match(Match {
                left: Expression::Named(NamedExpression::Meta(Meta {
                    key: MetaKey::Oifname,
                })),
                op: Operator::EQ,
                right: Expression::String(host_wan.clone().into()),
            }),
            // B. Match source address inside subnet 10.200.0.0/24
            Statement::Match(Match {
                left: Expression::Named(NamedExpression::Meta(Meta {
                    key: MetaKey::Iifname,
                })),
                op: Operator::EQ,
                right: Expression::String("br0".into()),
            }),
            // C. Target Action: Masquerade
            Statement::Masquerade(None),
        ]
        .into(),
        handle: None,
        index: None,
        comment: None,
    }));

    //add rule to forward inbound traffic safely
    batch.add(schema::NfListObject::Rule(schema::Rule {
        family: types::NfFamily::INet,
        table: table_name.clone().into(),
        chain: "forward_chain".into(),
        expr: vec![
            // A. Match input interface == $HOST_WAN
            Statement::Match(Match {
                left: Expression::Named(NamedExpression::Meta(Meta {
                    key: MetaKey::Iifname,
                })),
                op: Operator::EQ,
                right: Expression::String(host_wan.clone().into()),
            }),
            // B. Match output interface == br0
            Statement::Match(Match {
                left: Expression::Named(NamedExpression::Meta(Meta {
                    key: MetaKey::Oifname,
                })),
                op: Operator::EQ,
                right: Expression::String("br0".into()),
            }),
            // C. Match ct state IN [established, related]
            Statement::Match(Match {
                left: Expression::Named(NamedExpression::CT(expr::CT {
                    key: "state".into(),
                    family: None,
                    dir: None,
                })),
                op: Operator::IN, // Uses Operator::IN to match an array or set
                right: Expression::List(vec![
                    Expression::String("established".into()),
                    Expression::String("related".into()),
                ]),
            }),
            // D. Target Action: Accept
            Statement::Accept(None),
        ]
        .into(),
        handle: None,
        index: None,
        comment: None,
    }));

    //add rule to allow container outbount packets to escape
    batch.add(schema::NfListObject::Rule(schema::Rule {
        family: types::NfFamily::INet,
        table: table_name.clone().into(),
        chain: "forward_chain".into(),
        expr: vec![
            // A. Match input interface == br0
            Statement::Match(Match {
                left: Expression::Named(NamedExpression::Meta(Meta {
                    key: MetaKey::Iifname,
                })),
                op: Operator::EQ,
                right: Expression::String("br0".into()),
            }),
            // B. Match output interface == $HOST_WAN
            Statement::Match(Match {
                left: Expression::Named(NamedExpression::Meta(Meta {
                    key: MetaKey::Oifname,
                })),
                op: Operator::EQ,
                right: Expression::String(host_wan.clone().into()),
            }),
            // C. Target Action: Accept
            Statement::Accept(None),
        ]
        .into(),
        handle: None,
        index: None,
        comment: None,
    }));
}

fn apply_firewall_rules(batch: Batch) -> anyhow::Result<()> {
    let json_payload = batch.to_nftables();

    println!("[host firewall] Injecting netfilter rules via netlink...");

    match helper::apply_ruleset(&json_payload) {
        Ok(_) => {
            println!("[host firewall] Ruleset applied successfully! Container routing is live.");
            Ok(())
        }
        Err(e) => {
            eprintln!("[host firewall] Critical error injecting rules: {:?}", e);
            Err(e.into())
        }
    }
}

pub fn remove_firewall_rules(id: &Uuid) -> anyhow::Result<()> {
    let mut batch = Batch::new();
    let table_name = format!("mini-docker-{}", id.as_hyphenated());

    println!("[host firewall] removing all firewall rules");
    batch.delete(schema::NfListObject::Table(schema::Table {
        family: types::NfFamily::INet,
        name: table_name.into(),
        ..Default::default()
    }));
    let ruleset = batch.to_nftables();
    match helper::apply_ruleset(&ruleset) {
        Ok(_) => {
            println!("[host firewall] Firewall rules removed successfully");
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}
