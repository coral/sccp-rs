# Phone background images

`chan_sccp2` can apply a background image to a configured phone. Models with
selectable wallpapers receive an SCCP background-control document and download
the full image and thumbnail directly from an HTTP or HTTPS server. Legacy
XML-image models instead execute an image-service URL.

## Static resources

Configure the full image in the device section:

```ini
[SEP00A1B2C3D4E5]
type = device
background_image_url = http://assets.example.test/phones/desk.png
button = line, 1006
```

With no `background_thumbnail_url`, the module inserts `_thumb` before the
last extension. The example therefore uses
`http://assets.example.test/phones/desk_thumb.png`. A query string is preserved.
An extensionless `desk` becomes `desk_thumb`. A URL ending in `/` needs an
explicit thumbnail:

```ini
background_image_url = http://assets.example.test/render/
background_thumbnail_url = http://assets.example.test/render-thumbnail/
```

Both URLs must be absolute `http://` or `https://` URLs without credentials or
fragments and must be reachable from the phone. The module does not download,
proxy, resize, or inspect the resources. It does not enforce a filename
extension.

## Dynamic image servers

Dynamic mode expands one URL pattern after the phone registers. Selectable
background models receive full-image and thumbnail URLs. Legacy XML-image
models receive one display URL.

```ini
background_image_dynamic = yes
background_image_url = https://images.example.test/render.{FORMAT}?w={W}&h={H}&bitdepth={B}
```

`{W}` and `{H}` are required. `{FORMAT}` and `{B}` are optional. The supported
placeholders are case-sensitive and may appear more than once:

| Placeholder | Value |
| --- | --- |
| `{W}` | Requested width |
| `{H}` | Requested height |
| `{B}` | Phone display bit depth |
| `{FORMAT}` | Device image format: `png` or `xml` |

Do not configure `background_thumbnail_url` in dynamic mode. The registered
SCCP device type selects these values:

| Phone | Full image | Thumbnail | Bits | Format |
| --- | --- | --- | ---: | --- |
| 7906, 7911 | 95 x 34 | 23 x 8 | 1 | PNG |
| 7920 | 128 x 59 | none | 1 | Cisco image XML |
| 7940, 7960 | 133 x 65 | none | 2 | Cisco image XML |
| 7941, 7941G-GE, 7942, 7961, 7961G-GE, 7962 | 320 x 196 | 80 x 49 | 4 | PNG |
| 7970, 7971, IP Communicator | 320 x 212 | 80 x 53 | 12 | PNG |
| 7945, 7965 | 320 x 212 | 80 x 53 | 16 | PNG |
| 7975 | 320 x 216 | 80 x 53 | 16 | PNG |
| 7985 | 800 x 600 | 800 x 600 | 16 | PNG |
| 8941, 8945 | 640 x 480 | 123 x 111 | 24 | PNG |

Known device types without a compatible background-setting contract are left
unchanged. An undefined or unrecognized device type uses 320 x 212 for the
full image, 80 x 53 for the thumbnail, 16-bit depth, and PNG.

The 7920, 7940, and 7960 do not implement selectable backgrounds. For those
models, the module executes the generated URL and the server must return a
`CiscoIPPhoneImage` XML document. Only the image URL is used; a configured
static thumbnail has no effect on these models.

The configured background is applied after every successful registration and
when its effective URL pair changes during a reload. Removing the setting stops
future application; it cannot restore the firmware's built-in background.

## CLI overrides

The CLI accepts any configured device, including one that is currently
offline:

```text
sccp background SEP00A1B2C3D4E5 show
sccp background SEP00A1B2C3D4E5 set http://assets.example.test/phones/night.png
sccp background SEP00A1B2C3D4E5 set http://assets.example.test/phones/night.png http://assets.example.test/phones/night-small.png
sccp background SEP00A1B2C3D4E5 reset
```

`set` stores an override in Asterisk's internal database. It is applied
immediately when the phone is registered and otherwise on the next
registration. The override masks later `sccp.conf` changes until `reset`
removes it. `reset` uses the latest configured background, if present.

`show` reports whether the effective value comes from configuration or an
override, whether it is static or dynamic, how the thumbnail was selected, and
whether the phone is registered. URLs are deliberately omitted because their
paths and query strings can contain private tokens. CLI `set` creates a static
override; `reset` returns to the dynamic configuration when one is present.

A successful CLI update means the SCCP command was queued for the registered
phone. These phones do not report whether the later HTTP download and
application succeeded, so use the handset logs and HTTP server logs when
troubleshooting.
