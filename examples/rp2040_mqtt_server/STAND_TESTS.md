# GatoMQTT Stand Tests

This runbook covers the Step 11.4 stand tests for the current embedded example.

Important:
- The current example is wired for `CYW43` Wi-Fi, not `W5500`.
- TLS is not enabled yet on this step.
- Use plaintext MQTT on port `1883`.
- Credentials are currently:
  - username: `house`
  - password: `secret`

## 0. Build and flash

From this directory:

```powershell
cargo run
```

Expected serial logs:

```text
WiFi connected successfully
GatoMQTT broker initialized
GatoMQTT listening on port 1883
```

Capture the board IP address from logs and set it locally:

```powershell
$env:P4_IP = "192.168.1.50"
```

## 1. CONNECT + SUBSCRIBE + PUBLISH

Terminal 1:

```powershell
mosquitto_sub -h $env:P4_IP -p 1883 -u house -P secret -t test/topic -d
```

Terminal 2:

```powershell
mosquitto_pub -h $env:P4_IP -p 1883 -u house -P secret -t test/topic -m hello -d
```

Pass criteria:
- subscriber receives `hello`
- board stays connected

## 2. Retained message on SUBSCRIBE

Publish retained:

```powershell
mosquitto_pub -h $env:P4_IP -p 1883 -u house -P secret -t retain/topic -m retained-hello -r -d
```

Then subscribe:

```powershell
mosquitto_sub -h $env:P4_IP -p 1883 -u house -P secret -t retain/topic -C 1 -d
```

Pass criteria:
- subscriber immediately receives `retained-hello`

## 3. Eight concurrent clients

Open 8 terminals and run:

```powershell
mosquitto_sub -h $env:P4_IP -p 1883 -u house -P secret -i client-1 -t fanout/topic
mosquitto_sub -h $env:P4_IP -p 1883 -u house -P secret -i client-2 -t fanout/topic
mosquitto_sub -h $env:P4_IP -p 1883 -u house -P secret -i client-3 -t fanout/topic
mosquitto_sub -h $env:P4_IP -p 1883 -u house -P secret -i client-4 -t fanout/topic
mosquitto_sub -h $env:P4_IP -p 1883 -u house -P secret -i client-5 -t fanout/topic
mosquitto_sub -h $env:P4_IP -p 1883 -u house -P secret -i client-6 -t fanout/topic
mosquitto_sub -h $env:P4_IP -p 1883 -u house -P secret -i client-7 -t fanout/topic
mosquitto_sub -h $env:P4_IP -p 1883 -u house -P secret -i client-8 -t fanout/topic
```

Then publish:

```powershell
mosquitto_pub -h $env:P4_IP -p 1883 -u house -P secret -t fanout/topic -m fanout-ok
```

Pass criteria:
- all 8 subscribers receive `fanout-ok`
- no disconnect storm in board logs

## 4. Disconnect one client does not break others

With the 8 subscribers still running, close one terminal.

Then publish again:

```powershell
mosquitto_pub -h $env:P4_IP -p 1883 -u house -P secret -t fanout/topic -m after-one-left
```

Pass criteria:
- remaining 7 subscribers receive `after-one-left`
- board keeps accepting publishes

## 5. PINGREQ every 10s for 5 minutes

Run:

```powershell
mosquitto_sub -h $env:P4_IP -p 1883 -u house -P secret -i ping-client -t ping/topic -k 10 -d
```

Leave it connected for 5 minutes.

Pass criteria:
- no keepalive disconnect in board logs
- client stays connected for full 5 minutes

## 6. Control-domain style outbound via OUTBOUND_Q

The current example bridges command topics to outbound state echo:
- inbound `sb/.../set`
- outbound `sb/.../state`

Terminal 1:

```powershell
mosquitto_sub -h $env:P4_IP -p 1883 -u house -P secret -t sb/house1/device/relay-1/state -d
```

Terminal 2:

```powershell
mosquitto_pub -h $env:P4_IP -p 1883 -u house -P secret -t sb/house1/device/relay-1/set -m on -d
```

Pass criteria:
- board logs inbound command topic
- subscriber receives `on` on `.../state`

## Result template

Record the outcome like this:

```text
[ ] CONNECT + SUBSCRIBE + PUBLISH
[ ] retained on SUBSCRIBE
[ ] 8 concurrent clients
[ ] one client disconnect does not affect others
[ ] keepalive 5 minutes stable
[ ] OUTBOUND_Q delivery works
```

If any test fails, capture:
- board log excerpt
- exact mosquitto command used
- whether failure was immediate, intermittent, or only under 8-client load
