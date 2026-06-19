#!/usr/bin/env bash

# Exit immediately if a command exits with a non-zero status
set -e

# Configuration Variables
NS_NAME="runtime-test"
HOST_VETH="veth-test-host"
GUEST_VETH="veth-test-guest"
HOST_IP="192.168.50.1/24"
GUEST_IP="192.168.50.2/24"
GUEST_GW="192.168.50.1"

# Ensure the script is run as root
if [ "$EUID" -ne 0 ]; then
  echo "[-] Error: Please run this script with sudo."
  exit 1
fi

echo "[+] Initializing host simulation network sandbox..."

# 1. Clean up any leftover stale configuration from previous runs
if ip netns list | grep -q "$NS_NAME"; then
  echo "[!] Existing sandbox namespace found. Cleaning up..."
  ip netns del "$NS_NAME"
fi
if ip link show "$HOST_VETH" &>/dev/null; then
  ip link del "$HOST_VETH"
fi

# 2. Create the simulated host network namespace
echo "[+] Creating network namespace: $NS_NAME"
ip netns add "$NS_NAME"

# 3. Bring up the loopback interface inside the sandbox
ip netns exec "$NS_NAME" ip link set lo up

# 4. Create the VETH pair acting as the uplink cable
echo "[+] Creating virtual cable pair: $HOST_VETH <---> $GUEST_VETH"
ip link add "$HOST_VETH" type veth peer name "$GUEST_VETH"

# 5. Shove the guest end of the cable into the simulation sandbox
ip link set "$GUEST_VETH" netns "$NS_NAME"

# 6. Configure IP routing on the host side of the test tunnel
echo "[+] Configuring host-side IP address: $HOST_IP"
ip addr add "$HOST_IP" dev "$HOST_VETH"
ip link set "$HOST_VETH" up

# 7. Configure IP routing inside the isolated simulation sandbox
echo "[+] Configuring sandbox-side IP address: $GUEST_IP"
ip netns exec "$NS_NAME" ip addr add "$GUEST_IP" dev "$GUEST_VETH"
ip netns exec "$NS_NAME" ip link set "$GUEST_VETH" up

# 8. Establish the default gateway route inside the sandbox
echo "[+] Setting default routing gateway inside sandbox via $GUEST_GW"
ip netns exec "$NS_NAME" ip route add default via "$GUEST_GW"

# 9. Enable NAT on your actual host so the sandbox can hit the real internet
# Detect the active internet interface dynamically
REAL_WAN=$(ip route get 8.8.8.8 2>/dev/null | grep -oP 'dev \K\S+' || true)
if [ -n "$REAL_WAN" ]; then
  echo "[+] Enabling MASQUERADE on your real WAN interface ($REAL_WAN) for the test subnet"
  # Enable IP forwarding on your real system
  sysctl -w net.ipv4.ip_forward=1 >/dev/null

  # Setup standard translation rules using nftables (or fallback to iptables if nft isn't installed)
  if command -v nft &>/dev/null; then
    nft add table ip sandbox_nat_init || true
    nft add chain ip sandbox_nat_init postrouting { type nat hook postrouting priority 100 \; } || true
    nft add rule ip sandbox_nat_init postrouting oifname "$REAL_WAN" ip saddr 192.168.50.0/24 masquerade || true
  else
    iptables -t nat -A POSTROUTING -s 10.200.0.0/24 -o "$REAL_WAN" -j MASQUERADE || true
  fi
else
  echo "[!] Warning: No active internet interface detected. Sandbox will remain offline-only."
fi

echo -e "\n[+] Sandbox initialization complete!"
echo -e "[+] To execute your container runtime within this safe environment, run:"
echo -e "    \033[1;32msudo ip netns exec $NS_NAME cargo run\033[0m\n"
