# Running firmware with the network

How to give an unmodified firmware image a WiFi connection and let it reach the real network.
What is behind it: [wifi-plan.md](wifi-plan.md) (the MAC model and the virtual AP),
[networking-plan.md](networking-plan.md) (the subnet and the NAT).

## The short version

```sh
esp32sim --board none --boot rom --console usb --max-seconds 20 \
    --bootloader build/bootloader/bootloader.bin \
    --ptable build/partition_table/partition-table.bin \
    --app build/your_app.bin --elf build/your_app.elf \
    --wifi "ssid=esp32sim,psk=esp32sim-pass"
```

`--wifi` attaches the access point *and* the network behind it; there is nothing else to turn on.
The station comes up on **10.0.2.15/24**, gateway **10.0.2.2**, resolver **10.0.2.3**, and outbound
TCP/UDP is relayed to the host's own network (`--net nat`, the default).

The SSID and passphrase must match what the firmware is configured to look for — the emulator's AP
adopts whatever you give it. The AP is WPA2-PSK when `psk=` is present and open when it is not.

| Key | Meaning | Default |
| --- | --- | --- |
| `ssid=NAME` | network name the AP beacons | `esp32sim` |
| `psk=PASS` | WPA2-PSK passphrase; omit for an open network | none (open) |
| `chan=N` | channel in beacons and probe responses | 6 |
| `bssid=xx:xx:..` | AP MAC | `02:53:49:4d:00:01` |

`--net none` keeps the emulated subnet (DHCP, DNS, SNTP still answer) but refuses anything past the
gateway with an immediate RST, so applications fail fast instead of hanging — useful when you want a
run to be reproducible and offline.

`--hostfwd tcp:127.0.0.1:8080-:80` (repeatable) listens on the host and completes a TCP handshake
with a server in the guest: a browser on the Mac opens `http://127.0.0.1:8080/` and the station
sees a SYN from 10.0.2.2 to 10.0.2.15:80. Empty host address binds loopback; empty guest address is
the station. The listener is bound when the run starts; SYNs go out once the station's MAC is known
(association). Requires `--wifi` and `--net nat`.

To reach the guest from other machines (a phone, Home Assistant on the LAN), give the host address
explicitly: `--hostfwd tcp:0.0.0.0:6053-:6053` listens on every interface, so anyone who can reach
the host can reach the guest port; there is no filtering, so only do this on a network you trust.
Each connection appears to the guest as coming from 10.0.2.2.

`--mdns-relay HOSTIP` (or `auto`, the address the host's default route uses) makes the guest's
mDNS names work on the LAN, so Home Assistant discovers an ESPHome device and `cyd-emu.local`
resolves. The guest's mDNS responses (UDP to port 5353) are rewritten and multicast on the LAN
from a `0.0.0.0:5353` socket that joins 224.0.0.251 on HOSTIP's interface: A 10.0.2.15 becomes
HOSTIP, an SRV port with a `--hostfwd` becomes its host port (guest 3232 forwarded from host
13232 is published as 13232), and AAAA, other 10.0.2.x and 169.254.x addresses are dropped (and
AAAA cleared from NSEC). LAN queries for names and types the guest has announced are answered
from a cache of those records with their TTLs counting down; once a record is past half its TTL
(or gone), the query is passed into the guest instead, and its answer renews the cache. A
one-shot querier on another port (`dig -p 5353 @224.0.0.251`) gets a unicast answer with TTLs
capped at 10 s. The socket sets SO_REUSEADDR (and SO_REUSEPORT on Linux and macOS), so it shares
the port with Avahi, systemd-resolved or python-zeroconf. Publish the ports on `0.0.0.0`; a
`127.0.0.1` `--hostfwd` is advertised but warned about, since the LAN cannot reach it.

```sh
esp32sim --board cyd --flash-file flash.bin --wifi ssid=esp32sim,psk=esp32sim-pass --net nat \
  --hostfwd tcp:0.0.0.0:6053-:6053 --hostfwd tcp:0.0.0.0:3232-:3232 --mdns-relay auto --realtime
esphome logs cyd.yaml --device cyd-emu.local     # resolves to the host, port 6053
```

On **Windows** the same code uses Winsock's SO_REUSEADDR (there is no SO_REUSEPORT). The DNS
Client service's own mDNS responder, and Bonjour if installed, also hold UDP 5353; sharing works
as long as neither bound it with SO_EXCLUSIVEADDRUSE, otherwise the run stops with the bind
error. Every sharer gets each multicast datagram, but a unicast datagram to port 5353 reaches only
one of them, which is why the relay answers multicast. Windows Firewall asks to allow
`esp32sim.exe` on first use; allow private networks, or inbound UDP 5353 is dropped and only
announcements leave. This path compiles for Windows but has only been run on Linux.

The same `--wifi` and `--net` work on the **ESP32-C6** (`esp32sim-c6`): the access point and the
network are the same code, the MAC model is the C6's own ([wifi-c6-plan.md](wifi-c6-plan.md)). What
has been tried is one station on one open or WPA2 network; `examples/c6-wifi-station` has the full command. The C3 also supports `--wifi` with open/WPA2 station mode and the same network backend.

## The device's own hotspot (SoftAP setup portals)

Firmware that starts in `WIFI_MODE_AP` or `APSTA` (WiFiManager, openHASP, NerdMiner, ESPHome's
captive portal) expects a phone to join its hotspot and open a page at 192.168.4.1.
`--ap-client` is that phone. It hears the guest's beacons, authenticates and associates. If the
hotspot is WPA2, it runs the supplicant half of the four-way handshake with `psk=`. It then takes a
DHCP lease from the guest (normally 192.168.4.2). `--ap-fwd` publishes a guest port on the host
through the phone; it uses the same form as `--hostfwd`, but the empty guest address means 192.168.4.1.
These addresses exist only inside the emulator; nothing is sent to a real 192.168.4.x network.

    esp32sim --chip esp32 --board cyd-2432s028r --flash-file hasp.flash \
      --wifi ssid=esp32sim,psk=esp32sim-pass --net nat \
      --ap-client ssid=HASP-001122,psk=haspadmin --ap-fwd tcp:127.0.0.1:18480-:80

Every hotspot the guest advertises is printed as `[emu] device hotspot: 'NAME' bssid .. channel ..
(open|WPA2-PSK)`. Without `ssid=`, the phone joins the first open one; a secured one without
`psk=` is reported and skipped. IDF first beacons its default `ESP_xxxxxx` and then renames it on
the same BSSID; the phone notices the rename and starts the join again. Once a portal saves
credentials and reboots, the guest's station joins the `--wifi` network as usual, and the phone
rejoins whatever hotspot comes back. When the hotspot goes (reboot, AP mode off, channel change),
the phone closes every `--ap-fwd` connection at once, so the host side sees the end straight away; a
new connection made with no hotspot waits up to 10 s for one and is then reset. At run time, the web channel's `ap_client` / `ap_status`
messages do the same thing and list the hotspots ([web-ui.md](web-ui.md)).

Tried end to end:

| Firmware | Hotspot | Portal steps | Result |
| --- | --- | --- | --- |
| openHASP v0.7.0-rc12 | `HASP-xxxxxx`, **WPA2**, password `haspadmin` | `POST /api/config/wifi/` `{"ssid":"esp32sim","pass":"esp32sim-pass"}`, then `GET /config` (saves `/config.json` and reboots). A `GET /config` *before* the POST saves an empty SSID | reboots, `WIFI: Connected to esp32sim`, 10.0.2.15, `--hostfwd` port 80 serves its UI |
| NerdMiner v2 (WiFiManager) | `NerdMinerAP`, WPA2, `MineYourCoins` | send `Host: 192.168.4.1` (any other host gets WiFiManager's 302 captive redirect); `GET /wifisave?s=esp32sim&p=esp32sim-pass` with the form's Poolurl, Poolport, btcAddress, TimeZone fields | "Credentials saved", SW reset, "WiFi connected" |

Limits: the classic ESP32 only for now; the C3/C6 MACs do not deliver the phone's frames yet. One
phone, TCP forwards only, no UDP. The phone's frames set only the SoftAP bit (29) of the rx_ctrl
address-filter matches; bit 28 is the station's (esp32-open-mac's `filter_match`). Frames marked
for the station are dropped by the guest's AP.

## What the network gives the firmware

- **DHCP** — address, mask, gateway, DNS, so `esp_netif` reaches `IP_EVENT_STA_GOT_IP`.
- **DNS** — with NAT, queries go to the host's first `nameserver` from `/etc/resolv.conf` and the
  answer comes back looking as if 10.0.2.3 produced it. With `--net none`, the built-in responder
  answers every A query with 10.0.2.3 itself, so lookups succeed and the connection is then refused.
- **Time** — with NAT, SNTP requests reach the real server the firmware asked for. With `--net none`,
  the built-in SNTP server answers from the host clock (stratum 1), so firmware that waits for time
  still gets it in the first seconds.
- **ICMP echo** — answered inside the emulator for any address, including internet ones; pings never
  leave the host, so they are not a reachability test.
- **TCP/UDP out** — real connections to real hosts: an HTTP API, a Home Assistant instance on the
  LAN, MQTT, HTTPS (TLS runs on the emulated AES/SHA/RSA accelerators).

## Watching what happens

| Switch | Shows |
| --- | --- |
| `ESP_EMU_DEBUG_NET=1` | DHCP/ARP/ICMP/DNS exchanges and every NAT flow |
| `ESP_EMU_DEBUG_WIFI=1` | MAC-level events: descriptors, interrupts, TX queues |
| `ESP_EMU_DEBUG_WIFI_FRAMES=1` | every 802.11 frame on the air, decoded |
| `ESP_EMU_DEBUG_AES/SHA/RSA=1` | each accelerator operation as firmware requests it |
| end-of-run `[emu] wifi/net/nat/crypto` lines | frame, lease, flow, byte and operation counts |

## When it does not connect

- **`Send disconnect event, reason=210`** — the firmware has a passphrase compiled in and refuses an
  open AP (`NO_AP_FOUND_W_COMPATIBLE_SECURITY`). Pass a matching `psk=`.
- **No beacons seen / scan finds nothing** — check the SSID matches exactly, including case.
- **Connects but no IP** — look for the DHCP exchange with `ESP_EMU_DEBUG_NET=1`; if the four-way
  handshake did not finish, the station is not yet decrypting anything.
- **TLS fails or hangs** — the accelerators are the usual suspects; the `[emu] crypto:` counters
  show whether AES, SHA and RSA are all being exercised.
- **A host on the LAN is unreachable** — the NAT connects from the *host's* address, so the target
  must be reachable from the Mac, and any firewall there sees the Mac, not the guest.

## Known limits

- **UDP inbound** is not forwarded. `--hostfwd` is TCP only.
- **Multicast** does not cross the NAT, and mDNS only as `--mdns-relay` carries it: the guest's
  own responses go out and LAN queries for its names come in, but the guest's queries stay
  inside (it cannot resolve other `.local` names, and does not see a name conflict on the LAN:
  give each emulator its own name). IPv4 only, one guest, one interface.
- **UDP reply peers** must match the destination IP and port of the outgoing datagram. The
  [connected UDP relay](../esp-soc/src/nat.rs) does not support TFTP's server-selected transfer
  port or replies from a different address on a multi-homed server. Failed host sends drop
  the datagram without retrying and increment `udp_send_errors`,
  with details under `ESP_EMU_DEBUG_NET=1`. The 64-flow UDP table evicts its least recently
  active flow for a new destination or guest port and increments `udp_evicted`; an evicted
  flow's outstanding replies are lost.
- **One station**, no roaming, no power save, no 802.11n rates, no WPA3/SAE or PMF.
- **Traffic is not really encrypted** over the air — frames are plaintext framed as CCMP, which is
  what firmware sees anyway, but a capture of the emulated air is not a realistic WPA2 capture.
