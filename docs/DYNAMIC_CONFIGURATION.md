# Dynamic SCCP configuration through ARI

`chan_sccp2` can expose SCCP devices and lines as Asterisk Sorcery objects.
This lets an external controller manage PJSIP and SCCP through Asterisk's
standard dynamic-configuration API instead of a driver-specific endpoint.

The Sorcery configuration class is `chan_sccp2`; its object types are `device`
and `line`:

```text
HTTP:      /ari/asterisk/config/dynamic/chan_sccp2/{device|line}/{id}
WebSocket:      /asterisk/config/dynamic/chan_sccp2/{device|line}/{id}
Status variable: SCCP_CONFIG_STATUS
HTTP status query: /ari/asterisk/variable?variable=SCCP_CONFIG_STATUS
WebSocket status query: /asterisk/variable with variable=SCCP_CONFIG_STATUS
```

This mode is intended for deployments where a remote service
owns desired endpoint configuration. Asterisk keeps
the persisted desired objects and the last configuration that passed complete
validation locally, so an unavailable control plane does not prevent startup.

## Enable Sorcery mode

Keep listener, media, feature-policy, and soft-key-profile settings in
`sccp.conf`, and select the Sorcery inventory provider under `[general]`:

```ini
[general]
configuration_source = sorcery
bind = 0.0.0.0:2000
advertised_address = 192.0.2.10
server_name = Asterisk SCCP

[reception-softkeys]
type = softkey_profile
on_hook = redial, new_call
connected = hold, end_call, transfer
```

`configuration_source` defaults to `file`. In file mode, device and line
sections continue to come from `sccp.conf`. In Sorcery mode, device and line
sections in that file are not a second source of truth.

The module defaults both object types to Asterisk's writable AstDB Sorcery
wizard with the prefix `chan_sccp2`. An explicit equivalent mapping in
`sorcery.conf` is:

```ini
[chan_sccp2]
device = astdb,chan_sccp2
line = astdb,chan_sccp2
```

The explicit mapping is optional. It is useful when documenting local policy
or as the starting point for a deployment-specific Sorcery backend override.
Ensure `res_sorcery_astdb.so`, ARI, and the ARI Asterisk resource module are
available before loading `chan_sccp2`.

An empty Sorcery inventory is valid. This allows Asterisk to start before the
controller performs its first synchronization.

## Object fields

Scalar field names and values use the same canonical SCCP configuration names
as `sccp.conf`. Ordered or repeatable options use a dot followed by a numeric
index so that AstDB's JSON object representation does not collapse duplicate
keys. The index determines order; it need not describe a physical button
number.

```json
{
  "fields": [
    {"attribute": "description", "value": "Reception phone"},
    {"attribute": "button.0001", "value": "line, 1001, label=Main desk"},
    {"attribute": "button.0002", "value": "speed_dial, Helpdesk, 2000"}
  ]
}
```

The device `description` is shown in the primary line's upper-right station
header and may contain at most 39 bytes. A line button's `label` remains
independent and is shown beside that button.

Other repeatable settings, including codec operations, channel variables, and
feature defaults, use the same indexed form. Controllers should emit fixed-
width, monotonically increasing indexes (`0001`, `0002`, and so on) for stable
diffs. `GET` returns standard ARI `ConfigTuple` values and preserves the
indexed names.

ARI `PUT` patches a copy of the existing object: omitted attributes remain
unchanged. Set an indexed attribute such as `button.0002`, `allow.0001`, or
`setvar.0001` to an empty string to remove it. Empty ordinary scalar values
keep their normal SCCP configuration meaning.

## Provision with HTTP ARI

Create lines before devices that refer to them:

```sh
curl --fail-with-body --user "$ARI_USER:$ARI_PASSWORD" \
  --request PUT \
  --header 'Content-Type: application/json' \
  --data '{"fields":[
    {"attribute":"label","value":"Reception"},
    {"attribute":"context","value":"from-sccp"},
    {"attribute":"callerid","value":"Reception <1001>"}
  ]}' \
  http://127.0.0.1:8088/ari/asterisk/config/dynamic/chan_sccp2/line/1001

curl --fail-with-body --user "$ARI_USER:$ARI_PASSWORD" \
  --request PUT \
  --header 'Content-Type: application/json' \
  --data '{"fields":[
    {"attribute":"description","value":"Reception phone"},
    {"attribute":"button.0001","value":"line, 1001, label=Reception"}
  ]}' \
  http://127.0.0.1:8088/ari/asterisk/config/dynamic/chan_sccp2/device/SEP001122334455
```

Use `GET` on the same URLs to retrieve the complete normalized object. Delete
in the reverse dependency order: first the device, then a line that it no
longer references.

```sh
curl --fail-with-body --user "$ARI_USER:$ARI_PASSWORD" \
  --request DELETE \
  http://127.0.0.1:8088/ari/asterisk/config/dynamic/chan_sccp2/device/SEP001122334455

curl --fail-with-body --user "$ARI_USER:$ARI_PASSWORD" \
  --request DELETE \
  http://127.0.0.1:8088/ari/asterisk/config/dynamic/chan_sccp2/line/1001
```

## Provision over an outbound ARI WebSocket

Asterisk 22 and 23 support persistent outbound ARI WebSockets. Configure the
remote connection in `websocket_client.conf`, then associate it with an ARI
application and a local read/write ARI user in `ari.conf`. The remote peer can
send a `RESTRequest` on that same connection.

```ini
; websocket_client.conf
[ws-control]
type = websocket_client
connection_type = persistent
uri = wss://control.example.com/asterisk
protocols = ari
tls_enabled = yes
verify_server_cert = yes
verify_server_hostname = yes
reconnect_interval = 1000
enable_pingpongs = yes

; ari.conf
[sccp-control-user]
type = user
read_only = no
password = replace-with-a-local-secret

[ws-control]
type = outbound_websocket
websocket_client_id = ws-control
apps = sccp-control
subscribe_all = no
local_ari_user = sccp-control-user
```

The WebSocket server must accept the `ari` subprotocol. The credentials on the
`websocket_client.conf` connection authenticate Asterisk to the remote server;
`local_ari_user` separately controls which Asterisk REST operations the remote
peer may perform.

For example, the line create above becomes this WebSocket text message. The
`message_body` property is itself a JSON-encoded string:

```json
{
  "type": "RESTRequest",
  "transaction_id": "inventory-sync-42",
  "request_id": "line-1001-put",
  "method": "PUT",
  "uri": "/asterisk/config/dynamic/chan_sccp2/line/1001",
  "content_type": "application/json",
  "message_body": "{\"fields\":[{\"attribute\":\"label\",\"value\":\"Reception\"},{\"attribute\":\"context\",\"value\":\"from-sccp\"}]}"
}
```

Asterisk replies with a `RESTResponse` carrying the same `transaction_id` and
`request_id`, plus `status_code`, `reason_phrase`, and any response body. The
URI omits the HTTP `/ari` prefix because the message is already inside ARI.
See the machine-readable sequence in
[`asterisk-module/ci/sorcery/rest-over-websocket.requests.jsonl`](../asterisk-module/ci/sorcery/rest-over-websocket.requests.jsonl)
for line creation, device creation, indexed-field removal, reads, and
dependency-ordered deletion.

The ARI user's `read_only` setting still applies to requests received over an
outbound connection. Use a dedicated local ARI user with write access and
restrict the remote WebSocket with TLS and appropriate authentication.

## Reconciliation and failure behavior

Generic ARI dynamic configuration mutates one Sorcery object per request; it
does not provide a transaction spanning PJSIP objects, SCCP lines, and SCCP
devices. A controller should therefore use this order:

1. Create or update SCCP lines.
2. Create or update SCCP devices that reference those lines.
3. Remove obsolete SCCP devices.
4. Remove lines after no device references them.

The driver validates the complete SCCP inventory and applies it through the
same transactional reload path used by file and realtime configuration. An
invalid or incomplete desired inventory is retained for correction but is not
made active and does not replace the local last-known-good snapshot. A
successful ARI object write therefore confirms persistence of that individual
desired object, not runtime convergence.

`SCCP_CONFIG_STATUS` is the module-published convergence contract. Read it
through ARI's standard global-variable endpoint:

```sh
curl --fail-with-body --get --user "$ARI_USER:$ARI_PASSWORD" \
  --data-urlencode 'variable=SCCP_CONFIG_STATUS' \
  http://127.0.0.1:8088/ari/asterisk/variable
```

The ARI response wraps the variable value in `value`. Parse that value as a
second JSON document:

```json
{
  "generation": 6,
  "state": "converged",
  "operation": "update",
  "object_type": "device",
  "object_id": "SEP001122334455",
  "diagnostic": null
}
```

For an outbound WebSocket, send the equivalent request with structured query
parameters:

```json
{
  "type": "RESTRequest",
  "transaction_id": "inventory-sync-42",
  "request_id": "device-status",
  "method": "GET",
  "uri": "/asterisk/variable",
  "query_strings": [
    {"name": "variable", "value": "SCCP_CONFIG_STATUS"}
  ]
}
```

The controller must serialize SCCP mutations. Before a `PUT` or `DELETE`, read
the current `generation`. After the persistence response, poll the status until
the generation advances, the operation and object identity match the request,
and `state` is either `converged` or `failed`. `converged` means the complete
desired SCCP inventory became active. `failed` means the previous active
configuration remains in use; `diagnostic` contains the bounded reload error.
The observer receives no ARI transaction or request ID, so concurrent SCCP
writes cannot be correlated through Asterisk's generic Sorcery endpoint.
