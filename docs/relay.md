# Running the relay

The relay is how a phone reaches its Mac. The hub on the Mac dials out to it and registers its
channel; a phone dials the same relay; the relay splices the two WebSockets together and passes
binary messages between them unchanged. The hub and the phone run Noise end to end over those
messages, so the relay never holds a key and never sees a frame's contents. The design is
`docs/superpowers/specs/2026-11-19-relay-connectivity-design.md` (sections 4, 9 and 10); the code
is `crates/octet-relay`.

It is one small container: a single Rust binary, `octet-relay`, behind the hosting platform's
TLS proxy. It needs no database and no disk.

## What the relay sees

A relay, honest or not, sees each connection's address (or the proxy's), the channel ids hubs
register and phones ask for, connection ids, and the timing and size of every message. It can
drop, delay or cut a connection. It cannot read or change a frame (authenticated encryption),
pose as a hub (it lacks the identity key the phone pinned) or as a phone (it lacks a phone's
key), keep a departed phone looking present, or see a pairing link: those travel by QR code.

It logs counts and errors only: never a channel id, a connection id or a client address. Every
five minutes, and when it stops, it writes one line with the sockets and channels open against
their ceilings and what happened since the last line (registrations, phones, splices, refusals by
kind). The proxy in front is another matter: see [The proxy](#the-proxy).

## Exactly one instance

The channel table is in memory. Run **exactly one** instance, on a platform that allows
long-lived WebSockets and does not autoscale it. Two at once would split hubs from their
phones: a phone that lands on the instance its hub did not register with is told there is no
hub.

A deploy must **stop the old instance before it starts the new one** (on most platforms a
"recreate" or "stop first" strategy, not a rolling or blue-green one). What users see: every
connection drops when the old instance stops, and every hub and phone reconnects by itself on
the reconnect schedule (half a second, doubling to 30 seconds, with jitter). Nobody re-pairs, and
phones resume from their cursors. If the relay is down, no phone can connect; the desk is
unaffected.

## The proxy

The relay serves plain HTTP and WebSockets on `PORT` (8080 by default) on every interface, and
expects the platform's proxy to terminate TLS for it. Set up the proxy so that:

- **It does not log request paths.** Paths hold channel ids (`/v1/hub/<channel>`,
  `/v1/phone/<channel>`). Turn the proxy's request logging off, or strip the path from it.
- **Its idle timeout is above 60 seconds.** The relay pings every connection every 20 seconds,
  which keeps an idle splice open through any proxy whose timeout is longer than that; give it
  room (60 seconds or more).
- **It passes WebSocket upgrades** on `/v1/…`, and lets `GET /healthz` through for health checks.
- **It tells the relay the client's address**, in a header it always overwrites. Set
  `OCTET_RELAY_CLIENT_ADDRESS_HEADER` to that header's name (for example `Fly-Client-IP`, or
  `X-Forwarded-For`; when the header holds several addresses the relay takes the last, the one
  the proxy added). The per-address limits count by it. Unset, the relay counts by the socket's
  peer address, which behind a proxy is the proxy's, so every client would share one allowance.

Once the header is set, **the relay's port must be reachable only through the proxy**: anyone
who can reach the port directly can write the header and pick the address they are counted
under. Do not publish the container's port to the internet beside the proxy.

## Running it

Build the image from the repository root, tagged with the commit it was built from:

```sh
docker build -f Dockerfile.relay -t octet-relay:$(git rev-parse --short HEAD) .
```

Run it:

```sh
docker run -d --name octet-relay --restart unless-stopped -p 127.0.0.1:8080:8080 \
  -e OCTET_RELAY_CLIENT_ADDRESS_HEADER=X-Forwarded-For octet-relay:<tag>
```

The image runs the one binary as a non-root user (uid 10001). `PORT` changes the port inside the
container. The binary stops on `SIGTERM`, which drops every connection.

Check it from a machine outside the platform:

```sh
docker run --rm octet-relay:<tag> probe wss://relay.example.com
```

`octet-relay probe <url>` registers a throwaway channel under a fresh key, connects a phone
socket to it, opens the hub's data connection for that phone and passes a message each way. It
prints `ok` and exits 0, or prints which step failed and why and exits 1. `ws://host:port` works
for a relay without TLS (a local one, say).

## Pointing a hub at it

The hub uses the default relay unless `hub.json` (in the hub's data directory, beside
`devices.json`) names another:

```json
{
  "relay": "wss://relay.example.com"
}
```

It must be `wss://<host>` or `wss://<host>:<port>`, with no path. Phones keep the relay they
paired through: phones paired before the change must pair again, and `octet pair` says so.

## What to watch

- **The process is up**: the platform's health check on `GET /healthz` (`200 ok`). It shows the
  process answers, nothing more.
- **The probe**: `octet-relay probe wss://<host>` after every deploy, and on a schedule (every
  few minutes) from somewhere outside the platform, alerting when it exits non-zero.
- **Sockets against the ceiling**: the five-minute log line reads `sockets N of 20000, channels
  N of 5000`. Near the ceiling, new connections are refused (503 before the upgrade, 4429 after
  it) while established ones carry on.
- **Egress**: the platform's bandwidth graph. Each connection is read at up to 256 KiB a second
  after a 4 MiB burst, so a sustained climb means many busy phones, or someone using the relay as
  a pipe.

## Limits

The defaults (`crates/octet-relay/src/limits.rs`): messages up to 66,560 bytes; 16 phone sockets
per channel, waiting and spliced alike; 5,000 channels and 20,000 sockets across the relay; 30
phone connections and 10 registration attempts a minute per client address (an IPv6 address
counts by its /64); 256 KiB a second per connection after a 4 MiB burst, every message counting
(control messages, pings and pongs too); 10 seconds for a phone's hub to connect, for a hub to
answer its challenge, and for a client to send its request; a ping every 20 seconds, and a
connection that leaves two unanswered is dropped. Over a limit the newcomer is refused; an
established splice is never dropped for one.

## Updating and rolling back

Images are tagged with the commit they were built from; never deploy `latest`. To update, build
and push the new tag, then deploy it with the stop-first strategy and run the probe. To roll
back, deploy the previous tag the same way: the relay keeps no state, so any tag can follow any
other.

## Deploy checklist

1. Build the image at the commit to deploy: `docker build -f Dockerfile.relay -t
   octet-relay:<commit> .`, and push it to the platform's registry.
2. Confirm the service runs exactly one instance with autoscaling off, and deploys stop the old
   instance before starting the new.
3. Confirm the proxy terminates TLS, passes WebSocket upgrades, has an idle timeout of 60 seconds
   or more, and does not log request paths.
4. Confirm `OCTET_RELAY_CLIENT_ADDRESS_HEADER` names the header the proxy overwrites with the
   client's address, and that the relay's port is reachable only through the proxy.
5. Confirm the health check is `GET /healthz`.
6. Deploy the tag. Expect every hub and phone to drop and come back within half a minute.
7. Run `octet-relay probe wss://<host>` from outside the platform; it must print `ok`.
8. Open the app on a phone (on cellular) and check its Mac shows as connected.
9. Note the deployed tag, and the one before it to roll back to.
10. Make sure the scheduled outside probe is still running and alerting.
