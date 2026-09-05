# SCCP options

Compact reference for the `chan_sccp2` Asterisk module in this repository. It
describes accepted input, not old `chan_sccp` behavior. Unknown or wrong-scope
keys reject the complete configuration.

## Set and apply configuration

- Default file: `<Asterisk config directory>/sccp.conf` (normally
  `/etc/asterisk/sccp.conf`). Environment variable `SCCP_CONFIG` replaces that
  path exactly.
- File syntax: Asterisk INI, `key = value`; `;` starts an unquoted inline
  comment, `#` starts a whole-line directive/comment, and double quotes retain
  surrounding whitespace or semicolons. Keys and section types are ASCII
  case-insensitive. Use the canonical lowercase names below.
- Split files with `#include PATH` or optional `#tryinclude PATH`. The loaded
  module follows the host Asterisk parser's include rules. The standalone
  checker accepts bare, `"quoted"`, or `<bracketed>` paths, resolves relative
  paths beside the including file, caps nesting at 32 files, and rejects cycles.
- Validate: `chan-sccp2-config-checker /etc/asterisk/sccp.conf`.
- Require canonical key names: `chan-sccp2-config-checker --canonical FILE`.
- Emit canonical, template-expanded text: `chan-sccp2-config-checker normalize FILE`.
- Apply: `asterisk -rx 'sccp reload'`. Narrow forms are `sccp reload device ID`,
  `sccp reload line NUMBER`, and `sccp reload profile NAME`; a narrow reload is
  rejected if the candidate changes anything outside that target.
- Reload is transactional. These changes require a module restart:
  `configuration_source`, clear/TLS listeners, advertised/network/ACL/NAT
  policy, QoS, `keepalive`, `secondary_keepalive`, `server_name`, signaling
  failover, realtime table selection, and dial-terminator policy.
- There is no generic CLI `set` command. CLI-settable runtime state is listed
  under [CLI overrides](#cli-overrides); every other option is changed at its
  configuration source and then reloaded/restarted.

Notation used below: `bool` = `yes|no` (also `true|false|on|off|1|0`); `R` =
ordered/repeatable; `empty` means an empty right-hand side. Unless stated
otherwise, text is trimmed and must contain no control characters.

## Sources, sections, and precedence

```ini
[general]
key = value

[device-template](!)
type = device
key = value

[line-template](!)
type = line
key = value

[SEP001122334455](device-template)
type = device                    ; may be inherited
button = line, 1001

[1001](line-template)
type = line                      ; may be inherited

[desk-keys]
type = softkey_profile
on_hook = redial, new_call
```

- `[general]` is unique. Concrete sections are `type=device|line|softkey_profile`.
  A device ID is 1..15 ASCII alphanumerics, canonicalized uppercase. Every
  device needs at least one unique configured line; in file mode every line
  must be assigned.
- `[name](!)` declares a device/line template. `[name](parent1,parent2)` inherits
  parents left-to-right, then its own values. Scalar children replace inherited
  values. `allow`, `disallow`, `deny`, `permit`, `permit_host`, `setvar`, plus
  device `button`, `line`, and `feature_default`, append in order. A child with
  any `dnd_schedule` replaces the inherited schedule list; sole value `none`
  clears it.
- `configuration_source=file` (default): concrete devices/lines come from the
  file. `device_table` + `line_table` optionally overlay ordered Asterisk
  realtime rows on the file; both names are required and must differ.
- `configuration_source=sorcery`: general policy, templates, and soft-key
  profiles stay in the file; concrete devices/lines are Sorcery objects in
  class `chan_sccp2`, types `device` and `line`. Scalar attributes use the same
  names/values below. Repeatable fields use indexed names such as
  `button.0001`, `allow.0001`, `setvar.0001`, or `dnd_schedule.0001`. See
  [DYNAMIC_CONFIGURATION.md](DYNAMIC_CONFIGURATION.md).
- Effective call media precedence is line > device > general where that scope
  supports the option. Device ACL replaces general ACL when any device
  `deny`/`permit` is present; other device network/QoS scalar fields inherit
  independently.

## Shared value grammars

| Name | Accepted value |
| --- | --- |
| `bool` | `yes|no`; aliases listed above. |
| optional text | `empty`, and where noted `none|off|disabled`, clears it. |
| network | `internal` (RFC1918 IPv4 set), IPv4 `address/prefix`, IPv4 `address/netmask`, or IPv6 `address/prefix`; host bits normalize to the network. |
| NAT | `auto|off|(auto)off|on|(auto)on`; punctuation/case ignored. |
| DSCP | `0..63`, `none`, `EF`, `CS0..CS7`, `AF11..AF43`; also accepted legacy names `lowdelay|throughput|reliability|mincost`. |
| TOS | DSCP syntax is used directly; otherwise decimal `64..255` or hexadecimal `0x00..0xff` is converted to DSCP with `TOS >> 2`. Thus decimal `32` means DSCP 32, while `0x20` means DSCP 8. Do not combine a class's TOS and DSCP keys. |
| COS | `0..7`. |
| tone | Numeric `0..255` (decimal/hex), or: `Silence`, `Dtmf0..9`, `DtmfStar`, `DtmfPound`, `DtmfA..D`, `InsideDial`, `OutsideDial`, `LineBusy`, `Alerting`, `Reorder`, `RecorderWarning`, `RecorderDetected`, `Reverting`, `ReceiverOffHook`, `PartialDial`, `NoSuchNumber`, `BusyVerification`, `CallWaiting`, `Confirmation`, `CampOn`, `RecallDial`, `ZipZip`, `Zip`, `BeepBonk`, `Music`, `Hold`, `Test`, `MonitorWarning`, `AddCallWaiting`, `PriorityCallWaiting`, `BargeIn`, `DistinctAlert`, `PriorityAlert`, `ReminderRing`, `PrecedenceRingback`, `Preemption`, `NoTone`, `MeetMeGreeting`, `MeetMeNumberInvalid`, `MeetMeNumberFailed`, `MeetMeEnterPin`, `MeetMeInvalidPin`, `MeetMeFailedPin`, `MeetMeCfbFailed`, `MeetMeEnterAccessCode`, `MeetMeAccessCodeInvalid`, `MeetMeAccessCodeFailed`. Spaces, `_`, `-`, and case are ignored in named values. |
| audio encryption | `off`; or `optional,PROFILE...` / `required,PROFILE...`. Profiles: `aes-128-hmac-sha1-32`, `aes-128-hmac-sha1-80`, `f8-128-hmac-sha1-32`, `f8-128-hmac-sha1-80`, `aead-aes-128-gcm`, `aead-aes-256-gcm`. `off` forbids profiles; optional/required needs >=1. |
| codecs | Ordered `allow`/`disallow` operations over comma lists; prefix a token with `!` to invert that operation. `disallow=all` clears. `all` cannot share a comma list. Result needs >=1 audio codec, <=32 entries, and every included audio codec must have an Asterisk mapping. Tokens: `all,is11172,is13872,gsm,slin16,activevoice,alaw,ulaw,g722,g7221,g723,g726,g728,g729,ilbc,isac,opus,h224,aac,mp4alatm128,mp4alatm64,mp4alatm56,mp4alatm48,mp4alatm32,mp4alatm24,mp4alatmna,amr,amrwb,h261,h263,h264,h265,t120,data,t38fax,tote,xv711u,v711u,xv729a,v729a,clearchan,univxcoder,rfc2833,passthrough,dynamic,oob,rfc2833ib,cfb,noaudio,v150modem,v150sprt,v150sse`. |
| `setvar` | `NAME=value`; both nonempty, R, unique names, <=32 variables, name <=79 bytes, value <=1024 bytes, aggregate names+values <=8192 bytes. |

## `[general]`

Each row is `canonical key | default | accepted / effect`.

### Source, listener, network, and QoS

| Key | Default | Accepted / effect |
| --- | --- | --- |
| `configuration_source` | `file` | `file|sorcery`; selects concrete device/line provider. |
| `device_table` | unset | Realtime device family: 1..45 ASCII letters/digits/`_`; requires `line_table`, file mode only. |
| `line_table` | unset | Realtime line family, same grammar; must differ from `device_table`. |
| `bind` | `0.0.0.0:2000` | IPv4 `IP:PORT` or IPv6 `[IP]:PORT`; alternative to `bind_address`+`port`. |
| `bind_address` | `0.0.0.0` | IPv4/IPv6 clear-listener address; alternative form only. |
| `port` | `2000` | `1..65535`; alternative form only. |
| `advertised_address` | `127.0.0.1` | One non-unspecified, non-multicast IPv4/IPv6 address; legacy alternative to both family keys and clears the other family. |
| `advertised_ipv4` | `127.0.0.1` | Non-unspecified, non-multicast IPv4 or `none`/empty; cannot combine with `advertised_address`. |
| `advertised_ipv6` | unset | Non-unspecified, non-multicast IPv6 or `none`/empty; at least one advertised family must remain. |
| `tls_bind` | unset | TLS `IP:PORT`; alternative to `tls_bind_address`+`tls_port`. Any TLS setting enables the TLS listener. |
| `tls_bind_address` | `0.0.0.0` when TLS requested | IPv4/IPv6 TLS bind address. |
| `tls_port` | `2443` when TLS requested | `1..65535`; TLS and clear sockets must differ. |
| `tls_combined_pem` | unset | Nonempty PEM path containing certificate+key; exclusive with split credentials. |
| `tls_certificate` | unset | Certificate path; requires `tls_private_key`. |
| `tls_private_key` | unset | Private-key path; requires `tls_certificate`. |
| `tls_trust_store` | unset | Optional CA path; valid only with split certificate/key. |
| `deny` / `permit` (R) | empty ACL (no filter) | Ordered ACL action + `network`; empty occurrence clears accumulated rules. Retained/reload-tracked, but the current runtime does not yet consult this ACL for connection admission. |
| `localnet` (R) | `10/8,172.16/12,192.168/16` | Local `network`; empty occurrence clears accumulated networks. |
| `externip` | unset | Fixed non-unspecified IPv4/IPv6 external address or `none`/empty; exclusive with `externhost`. |
| `externhost` | unset | DNS hostname <=253 bytes or `none`/empty; exclusive with `externip`. |
| `externrefresh` | `60` with `externhost` | DNS refresh seconds `1..86400`; invalid without `externhost`. |
| `nat` | `auto` | NAT grammar above. |
| `sccp_dscp` / `sccp_cos` | `24 (CS3)` / `4` | Signaling DSCP/COS. `sccp_tos` is the mutually exclusive legacy TOS form. |
| `audio_dscp` / `audio_cos` | `46 (EF)` / `6` | Audio DSCP/COS. `audio_tos` is the mutually exclusive legacy TOS form. |
| `video_dscp` / `video_cos` | `34 (AF41)` / `5` | Video DSCP/COS. `video_tos` is the mutually exclusive legacy TOS form. |

### Identity, station UI, dialing, and registration

| Key | Default | Accepted / effect |
| --- | --- | --- |
| `server_name` | `Asterisk SCCP` | Name presented by the SCCP server. |
| `language` | `en` | Nonempty printable PBX language <=63 bytes; inherited by lines. |
| `accountcode` | unset | Printable CDR account code <=79 bytes; empty clears; inherited by lines. |
| `keepalive` | `30` | Seconds, `5..4294967295`. |
| `secondary_keepalive` | `30` | Seconds, `5..4294967295`. |
| `signaling_server` (R) | none | `priority,name,address,clear-port-or-none,secure-port-or-none`; <=5 routes, priority `1..255` unique, name 1..47 bytes, address non-unspecified/non-multicast, at least one nonzero port. If present, priorities must include `server_priority`. |
| `first_digit_timeout` | `10` | Seconds `1..86400`. |
| `interdigit_timeout_ms` | unset | Milliseconds `250..86400000`; exclusive with `digit_timeout`. |
| `digit_timeout` | `5` | Seconds `1..86400`; exclusive legacy-duration form of interdigit timeout. |
| `digit_timeout_char` | `#` | One DTMF character: `0..9`, `*`, `#`, `A..D`. |
| `record_digit_timeout_char` | `no` | `bool`; include terminator in collected number. |
| `simulate_enbloc` | `yes` | `bool`; defer routing until number collection completes. |
| `speed_dial_await_further_digits` | `no` | `bool`; seed collection from a speed dial instead of routing immediately. |
| `allow_overlap` | `no` | `bool`; default device overlap dialing. |
| `transfer_on_hangup` | `no` | `bool`; handset hangup completes an eligible attended transfer. |
| `call_answer_order` | `OldestFirst` | `OldestFirst|LastFirst`. |
| `dateformat` | `D/M/Y` | Exactly `D`, `M`, and `Y|YY`, each once, with two `/|.|-|space` separators; optional trailing `A` selects 12-hour clock; <=7 bytes. |
| `tzoffset` | `0` | Whole UTC offset hours `-14..14`; affects phone display, not DND schedule timezone. |
| `ring_type` | `Outside` | `Off|Inside|Outside|Feature|Silent|Urgent|Bellcore1..Bellcore5`. |
| `call_waiting_tone` | `CallWaiting` | `tone`; numeric `0` disables it. |
| `call_waiting_interval` | `0` | Repeat seconds `0..86400`; `0` means initial tone only. |
| `fallback` | `no` | `yes|no|odd|even`; registration-token move-back decision. |
| `backoff_time` | `60` | Registration-token backoff seconds `30..86400`. |
| `server_priority` | `1` | `1..255`; local server priority. |
| `regcontext` | empty | Unique `&`-separated context names, total <=79 bytes; empty disables registration extensions. |

### Media and call features

| Key | Default | Accepted / effect |
| --- | --- | --- |
| `allow` / `disallow` (R) | all mapped audio codecs | Codec operations above. Supplying any operation starts this scope from empty. |
| `audio_encryption` | `off` | Audio-encryption grammar; inherited by devices and lines. |
| `direct_media` | `no` | `bool`; permit direct RTP. |
| `early_media` | `yes` | `bool`; compatibility enabled values: `offhook|immediate|dial|ringout|progress`; `none` disables. |
| `echocancel` | `yes` | `bool`; inherited by lines. |
| `silencesuppression` | `no` | `bool`; inherited by lines. |
| `jb_enable` | `no` | `bool`; Asterisk receive jitter buffer. |
| `jb_force` | `no` | `bool`; force jitter buffer even with direct media. |
| `jb_log` | `no` | `bool`; jitter-frame logging. |
| `jb_max_size` | `200` | Milliseconds `1..2147483647`. |
| `jb_resync_threshold` | `1000` | Milliseconds `1..2147483647`. |
| `jb_implementation` | `fixed` | `fixed|adaptive`. |
| `meetme` | `yes` | `bool`; default destination-based conference dialing, inherited by devices. |
| `meetmeopts` | `qxd` | Printable conference application option string; empty allowed; inherited by devices. |
| `autoanswer_ring_time` | `1` | Ring delay seconds `0..4294967295`. |
| `autoanswer_tone` | `Zip` | `tone`. |
| `remote_hangup_tone` | disabled | `tone`; numeric `0` disables passive remote-hangup notification. |
| `hotline_enabled` | `no` | `bool`; allow unknown devices to register on the guest hotline. |
| `hotline_extension` | `111` | Printable destination <=79 bytes; empty clears. |
| `hotline_context` | `default` | Printable text <=79 bytes; empty allowed only while hotline disabled. |
| `hotline_label` | `hotline` | Printable text <=79 bytes; empty allowed only while hotline disabled. |

## Device sections

Form: `[DEVICE_ID]`, `type = device`. Defaults below are after general
inheritance and before durable CLI/handset state is restored.

| Key | Default | Accepted / effect |
| --- | --- | --- |
| `type` | required/inherited | Must be `device`. |
| `description` | device ID | Station header, <=39 bytes, no controls. |
| `softkey_profile` | `default` | Existing profile name; names are trimmed/case-folded. |
| `button` / `line` (R) | none; >=1 line required | Ordered physical layout; see [Buttons](#buttons). `line=NUMBER,...` is shorthand for `button=line,NUMBER,...`. Max logical/expanded layout: 256 buttons; ordinary per-namespace instances: 1..255. |
| `setvar` (R) | none | Channel variable grammar; device values apply before line values. |
| `cfwdall` | `yes` | `bool`; enable forward-all control. |
| `cfwdbusy` | `yes` | `bool`; enable forward-busy control. |
| `cfwdnoanswer` | `yes` | `bool`; enable forward-no-answer control. |
| `forward_no_answer_timeout` | `30` | Seconds `1..86400`. |
| `forward_all` | unset | Initial destination <=23 printable bytes; `empty|none|off|disabled` clears. |
| `forward_busy` | unset | Same. |
| `forward_no_answer` | unset | Same. |
| `dnd_feature` | `yes` | `bool`; enable manual DND UI/control. Does not disable schedules. |
| `dnd` | `off` | Initial `off|silent|reject`; aliases `none|disabled` -> off, `busy` -> reject. |
| `dnd_schedule` (R) | none | `HH:MM-HH:MM, DAYS, silent|reject`; <=32 entries, <=128 bytes each, no weekly overlap. `DAYS=*|mon..sun|RANGE`, joined with `&`; ranges may wrap. Start inclusive, end exclusive; `24:00` only as end; server local timezone. Sole `none` clears inherited list. |
| `background_image_dynamic` | no | When enabled, `background_image_url` is a case-sensitive pattern requiring `{W}` and `{H}` and optionally using `{FORMAT}` and `{B}`; the registered phone model supplies full-size and thumbnail values, with `{FORMAT}` resolving to `png` or `xml`. |
| `background_image_url` | unset | Absolute `http://` or `https://` URL, <=256 characters, valid `%HH` escapes, no credentials/fragment/whitespace/control/backslash; `empty|none|off|disabled` clears. In dynamic mode this is the URL pattern. The image/thumbnail pair must fit the phone XML control document. |
| `background_thumbnail_url` | unset; derived when an image is set | Same URL grammar; invalid without image URL and forbidden in dynamic mode. Empty clears the explicit thumbnail and derives one by inserting `_thumb` before the extension/query; a URL with no filename needs an explicit thumbnail. |
| `privacy_feature` | `yes` | `bool`; enable privacy UI/control. |
| `privacy` | `no` | `bool`; initial device privacy. |
| `feature_default` (R) | `off` per feature button | `FEATURE_INSTANCE,bool`; instance >=1 must exist. |
| `park` | `yes` | `bool`; enable parking. |
| `conf_allow` | `yes` | `bool`; permit conference control. |
| `conf_music_on_hold_class` | `default` | Nonempty printable class; empty disables conference MOH. |
| `conf_play_general_announce` | `yes` | `bool`. |
| `conf_play_part_announce` | `yes` | `bool`. |
| `conf_mute_on_entry` | `no` | `bool`. |
| `conf_show_conflist` | `yes` | `bool`. |
| `meetme` | general value | `bool`; device conference-dialing override. |
| `meetmeopts` | general value | Printable application options; empty allowed. |
| `use_redial_menu` | `no` | `bool`; `yes` opens Placed Calls rather than dialing the last number. |
| `allow_ringin_notification` | `no` | `bool`; ringing notification for hinted lines. |
| `mwi_lamp` | `on` | `off|on|wink|flash|blink`. |
| `mwi_on_call` | `no` | `bool`; retain MWI during calls. |
| `phone_code_page` | `ISO8859-1` | `ISO8859-1|Latin1|ASCII|US-ASCII`; used only for legacy non-UTF-8 phones. |
| `allow_overlap` | general value | `bool`. |
| `force_dtmf_mode` | `auto` | `auto|rfc2833|skinny`. |
| `direct_media` | general value | `bool`. |
| `early_media` | general value | Same grammar as general. |
| `audio_encryption` | general value | Audio-encryption grammar. |
| `allow` / `disallow` (R) | general codecs | Codec operations; supplying any operation starts this scope from empty. |
| `deny` / `permit` (R) | general ACL | Ordered ACL; any device occurrence replaces inherited general ACL. Empty occurrence clears accumulated device/template rules. Currently not enforced for connection admission. |
| `permit_host` (R) | none | Unique DNS hostname <=253 bytes; empty clears inherited hosts. Retained/reload-tracked but currently not enforced. |
| `nat` | general value | NAT grammar. |
| `transport` | `either` | `clear|tls|either`; aliases by value: `tcp|secure|any`. `tls` requires configured general TLS. |
| `sccp_dscp` / `sccp_cos` | general values | Device signaling DSCP/COS; legacy alternative `sccp_tos`. |
| `audio_dscp` / `audio_cos` | general values | Device audio DSCP/COS; legacy alternative `audio_tos`. |
| `video_dscp` / `video_cos` | general values | Device video DSCP/COS; legacy alternative `video_tos`. |

## Buttons

Comma fields are trimmed. Button declarations and `line` shorthand share one
ordered layout. `KIND`, feature names, and option names ignore case and
non-alphanumeric punctuation.

| Syntax | Meaning / constraints |
| --- | --- |
| `button = line, NUMBER[,label=TEXT][,caller_name=TEXT][,caller_number=TEXT][,ring=normal|silent|disabled][,subscription=EXT@CONTEXT][,privacy=bool]` | Appearance of an existing line. Option aliases: `ringmode`, `subscriptionidentity`; `off` aliases disabled. Each logical line may occur only once per device. |
| `line = NUMBER[,same options]` | Line-button shorthand; retains declaration order among `button` entries. |
| `button = speed_dial, LABEL, NUMBER` | Ordinary speed dial. |
| `button = speed_dial, LABEL, NUMBER, EXT@CONTEXT` | BLF speed dial (fourth field is an Asterisk hint, not a PJSIP device name). |
| `button = blf, LABEL, NUMBER, EXT@CONTEXT` | Explicit BLF; `blfspeeddial` alias. |
| `button = feature, LABEL, FEATURE[,ARG...]` | Feature button. LABEL is nonempty; keep its station encoding <=39 bytes for every phone (dynamic-feature phones support 120). `monitor` is recording, strictly limits LABEL to 39 UTF-8 bytes/no controls, and accepts no ARG. |
| `button = service, LABEL, HTTP(S)-URL` | Phone service; URL may contain commas because remaining fields rejoin. Label <=39 bytes; URL <=255 bytes, no fragment, <=32 nonempty query parameters, each name/value <=128 bytes. |
| `button = addon, SLOT, MODEL` | Unique sidecar slot `1..56`; kind alias `addon_module`. Models: `7914`, `7915-12`, `7915-24`, `7916-12`, `7916-24`, `SPA500S`, `SPA500DS`, `SPA932DS` (Cisco/addon-prefixed normalized aliases accepted). Following buttons consume its capacity. |
| `button = empty` | Unused physical position; alias `unused`. |

Feature names:

```text
redial(lastnumberredial), hold, transfer,
forward_all(cfwdall), forward_busy(cfwdbusy),
forward_no_answer(cfwdnoanswer), video, voicemail, answer_release,
auto_answer, select, feature, malicious_call, meetme(meetmeconference),
conference, park(callpark), pickup(callpickup),
group_pickup(groupcallpickup), mobility, dnd(donotdisturb),
conference_list, remove_last_participant, quality_report(qualityreporttool),
callback, other_pickup, video_mode, new_call, end_call,
hunt_group_login, queue(queuing), parkinglot, messages, directory,
application, headset, echo_cancellation(acousticechocancellation), monitor
```

Only these feature arguments have typed meaning:

- DND: omit ARG to cycle `off -> reject -> silent`; ARG `silent|reject`
  (`busy` aliases reject) makes a fixed-mode toggle.
- Parking lot: ARG `LOT[,RetrieveSingle|AlwaysShowMenu]`; omitted ARG is
  `default,RetrieveSingle`.
- `monitor`: no ARG; creates the recording control documented in
  [RECORDING.md](RECORDING.md).

## Line sections

Form: `[LINE_NAME]`, `type = line`. A station line number is nonempty and <=24
bytes. A logical line may be shared by multiple devices.

| Key | Default | Accepted / effect |
| --- | --- | --- |
| `type` | required/inherited | Must be `line`. |
| `label` | section/line name | Default station-visible line label. |
| `context` | `from-sccp` | Nonempty printable dialplan context. |
| `callerid` | line name for both | `"NAME" <NUMBER>`; without angle brackets the entire value is both name and number. |
| `incoming_limit` | `6` | Concurrent inbound logical calls `0..255`. |
| `language` | general value | Nonempty printable PBX language <=63 bytes. |
| `accountcode` | general value | Printable CDR account code <=79 bytes; empty clears. |
| `setvar` (R) | none | Channel-variable grammar; replaces same-named device variable on outbound channels. |
| `mailbox` | unset | `MAILBOX` or `MAILBOX@CONTEXT`, no whitespace; `empty|none|off|disabled` clears. |
| `voicemail_number` | unset | Printable voicemail destination <=79 bytes; `empty|none|off|disabled` clears. |
| `voicemail_transfer` | unset | Printable transfer-to-voicemail destination <=79 bytes; same clear values. |
| `call_group` | empty | Unique comma values/ranges `0..63`, e.g. `1,3-5`; empty clears. |
| `pickup_group` | empty | Same. |
| `named_call_group` | empty | Unique comma-separated nonempty names; empty clears. |
| `named_pickup_group` | empty | Same. |
| `directed_pickup` | `yes` | `bool`. |
| `directed_pickup_context` | line context | Nonempty printable context; `empty|none|off|disabled` restores line context. |
| `pickup_mode_answer` | `yes` | `bool`; answer after directed pickup. |
| `parkinglot` | unset | Printable Asterisk parking-lot name; empty clears. |
| `meetme` | inherit device | `bool`; if explicitly `yes`, `meetmenum` is required; `no` conflicts with number/options. |
| `meetmenum` | unset | Conference destination; empty clears. |
| `meetmeopts` | inherit device | Printable application options; empty is an explicit empty override. |
| `adhoc_number` | unset | Off-hook hotline destination <=79 printable bytes; empty clears. |
| `initial_dialtone_tone` | `InsideDial` | `tone`. |
| `secondary_dialtone_digits` | unset | Empty disables; otherwise 1..9 DTMF characters `0..9*#A..D`. |
| `secondary_dialtone_tone` | `OutsideDial` | `tone`. |
| `pin` | unset | Empty disables Extension Mobility PIN; otherwise 1..7 ASCII digits. |
| `regexten` | line name | Unique `&`-separated `EXT` or `EXT@CONTEXT`, total <=255 bytes, each part <=79 with no whitespace/`&`/extra `@`. Requires general `regcontext`; omitted entries expand the line name into every `regcontext`. Empty restores that default. Resolved targets must be globally unique. |
| `allow` / `disallow` (R) | general codecs | Codec operations; supplying any operation starts this scope from empty. |
| `video_mode` | `auto` | `off|user|auto`. |
| `audio_encryption` | general value | Audio-encryption grammar. |
| `echocancel` | general value | `bool`. |
| `silencesuppression` | general value | `bool`. |

## Soft-key profile sections

```ini
[profile-name]
type = softkey_profile
connected = hold, end_call, transfer
ring_in = answer, end_call
```

Keys: `on_hook`, `connected`, `on_hold`, `ring_in`, `off_hook`,
`connected_transfer`, `digits_following`, `connected_conference`, `ring_out`,
`off_hook_feature`, `in_use_hint`, `on_hook_stealable`, `hold_conference`,
`empty`. Each value is an ordered comma list with <=16 unique actions. Empty or
omitted means no actions in that mode.

Actions:

```text
redial, new_call, hold, transfer, forward_all(cfwdall),
forward_busy(cfwdbusy), forward_no_answer(cfwdnoanswer), backspace,
end_call, resume, answer, info, conference, park, join, meetme, pickup,
group_pickup, monitor, callback, barge, dnd(do_not_disturb),
conference_list, select, private, transfer_to_voicemail, direct_transfer,
immediate_divert, video_mode, intercept, empty, dial
```

The built-in `default` profile is:

```text
on_hook=new_call
connected=hold,end_call,transfer
on_hold=resume,new_call,end_call
ring_in=answer,end_call
off_hook=end_call
connected_transfer=hold,end_call,transfer
digits_following=backspace,end_call,dial
connected_conference=hold,end_call
ring_out=end_call
off_hook_feature=resume,new_call,end_call
on_hook_stealable=intercept,new_call
hold_conference=resume,new_call,end_call
in_use_hint= ; empty
empty=       ; empty
```

## Compatibility key aliases

Prefer the canonical name on the left. Aliases are case-insensitive but may
not be combined with their canonical form or another alias for the same
semantic slot.

### General

```text
bind <- clearbind
bind_address <- bindaddr, clearbindaddr
port <- clearport
advertised_ipv4 <- advertisedaddressipv4
advertised_ipv6 <- advertisedaddressipv6
tls_bind <- securebind
tls_bind_address <- secbindaddr, tlsbindaddr
tls_port <- secport, tlsport
tls_combined_pem <- certfile, tlscombinedpem
tls_certificate <- tlscertificatefile
tls_private_key <- tlsprivatekeyfile
tls_trust_store <- tlscafile
externip <- externaladdress
externhost <- externalhost
externrefresh <- externalrefresh
sccp_tos <- signalingtos
sccp_dscp <- sccpdscp, signalingdscp, signaling_dscp
sccp_cos <- signalingcos, signaling_cos
audio_tos <- audiotos
audio_dscp <- audiodscp
audio_cos <- audiocos
video_tos <- videotos
video_dscp <- videodscp
video_cos <- videocos
server_name <- servername
first_digit_timeout <- firstdigittimeout
digit_timeout <- digittimeout
digit_timeout_char <- digittimeoutchar
record_digit_timeout_char <- recorddigittimeoutchar
speed_dial_await_further_digits <- speeddialawaitfurtherdigits
allow_overlap <- allowoverlap
call_answer_order <- callanswerorder
ring_type <- ringtype
call_waiting_tone <- callwaitingtone
call_waiting_interval <- callwaitinginterval
autoanswer_ring_time <- autoanswerringtime
autoanswer_tone <- autoanswertone
remote_hangup_tone <- remotehangup_tone
hotline_enabled <- hotlineenabled
hotline_extension <- hotlineextension
hotline_context <- hotlinecontext
hotline_label <- hotlinelabel
direct_media <- directrtp
early_media <- earlyrtp
audio_encryption <- audioencryption
jb_enable <- jbenable
jb_force <- jbforce
jb_log <- jblog
jb_max_size <- jbmaxsize
jb_resync_threshold <- jbresyncthreshold
jb_implementation <- jbimpl
device_table <- devicetable
line_table <- linetable
```

### Device

```text
softkey_profile <- softkeyprofile
cfwdall <- forwardallenabled, forward_all_enabled
cfwdbusy <- forwardbusyenabled, forward_busy_enabled
cfwdnoanswer <- forwardnoanswerenabled, forward_no_answer_enabled
forward_no_answer_timeout <- cfwdnoanswertimeout, forwardnoanswertimeout
dnd_feature <- dndfeature
privacy_feature <- private, privacyfeature
feature_default <- featuredefault
conf_allow <- confallow, conference_allow
conf_music_on_hold_class <- confmusiconholdclass, conference_music_on_hold_class
conf_play_general_announce <- confplaygeneralannounce, conference_play_general_announce
conf_play_part_announce <- confplaypartannounce, conference_play_participant_announce
conf_mute_on_entry <- confmuteonentry, conference_mute_on_entry
conf_show_conflist <- confshowconflist, conference_show_list
use_redial_menu <- useredialmenu
allow_ringin_notification <- allowringinnotification
mwi_lamp <- mwilamp
mwi_on_call <- mwioncall
phone_code_page <- phonecodepage
allow_overlap <- allowoverlap
force_dtmf_mode <- forcedtmfmode, force_dtmfmode
direct_media <- directrtp
early_media <- earlyrtp
audio_encryption <- audioencryption
permit_host <- permithost
transport <- transportrequirement, transport_requirement
sccp_tos <- signalingtos
sccp_dscp <- sccpdscp, signalingdscp, signaling_dscp
sccp_cos <- signalingcos, signaling_cos
audio_tos <- audiotos
audio_dscp <- audiodscp
audio_cos <- audiocos
video_tos <- videotos
video_dscp <- videodscp
video_cos <- videocos
```

### Line

```text
incoming_limit <- incominglimit
voicemail_number <- vmnum, voicemailnumber
voicemail_transfer <- trnsfvm, voicemailtransfer, transfertovoicemail
call_group <- callgroup
pickup_group <- pickupgroup
named_call_group <- namedcallgroup
named_pickup_group <- namedpickupgroup
directed_pickup <- directedpickup
directed_pickup_context <- directedpickupcontext
pickup_mode_answer <- pickupmodeanswer, directedpickupmodeanswer
adhoc_number <- adhocnumber
video_mode <- videomode
audio_encryption <- audioencryption
```

Removed inputs are recognized only to produce an error: general/device
`trust_phone_ip` (`trustphoneip`) must be removed because peer addresses are
always authoritative; device `dtmfmode` must become `force_dtmf_mode`.

## CLI overrides

These mutate live/durable device state; they do not rewrite `sccp.conf`.

| File default | CLI | Persistence / precedence |
| --- | --- | --- |
| device `dnd` | `sccp dnd DEVICE off\|silent\|reject` | Stored feature state; manual value wins until another manual/scheduled transition. Requires configured device and enabled feature. |
| device `dnd_schedule` | `sccp dnd schedule DEVICE show`<br>`... add HH:MM-HH:MM DAYS silent\|reject`<br>`... remove ONE_BASED_INDEX`<br>`... clear`<br>`... reset` | `add/remove` copy then edit effective rules; `clear` stores an empty override; override masks later file changes; `reset` deletes it and uses current config. Works while device is offline. |
| device background URLs | `sccp background DEVICE show`<br>`... set IMAGE_URL [THUMB_URL]`<br>`... reset` | `set` persists and masks config; `reset` deletes override and reapplies current config. Works while offline; delivery occurs now or on registration. |
| device `forward_all|forward_busy|forward_no_answer` | `sccp set forwarding DEVICE LINE all\|busy\|noanswer DESTINATION\|off` | Persists device-wide selected forwarding kind after validating that LINE is its appearance and feature is enabled. `off` clears it. |

## Complete Asterisk CLI surface

Commands can also be sent noninteractively as `asterisk -rx 'COMMAND'`.

```text
sccp version
sccp reload [device ID|line NUMBER|profile NAME]
sccp show devices [DEVICE [appearances [DEVICE:INSTANCE]|buttons [POSITION]|capabilities [POSITION]|features [NAME]]]
sccp show lines [LINE [appearances [DEVICE:INSTANCE]]]
sccp show channels [PBX_CALL_ID]
sccp show media [PBX_CALL_ID [CALL_ID [audio|video [receive|transmit]]]]
sccp show media statistics [DEVICE [CALL_ID]]
sccp show sessions [DEVICE]
sccp reset DEVICE|all
sccp restart DEVICE|all
sccp dnd DEVICE off|silent|reject
sccp dnd schedule DEVICE show
sccp dnd schedule DEVICE add HH:MM-HH:MM DAYS silent|reject
sccp dnd schedule DEVICE remove ONE_BASED_INDEX
sccp dnd schedule DEVICE clear
sccp dnd schedule DEVICE reset
sccp background DEVICE show
sccp background DEVICE set IMAGE_URL [THUMBNAIL_URL]
sccp background DEVICE reset
sccp message DEVICE|all|system TEXT [yes|no] [TIMEOUT_SECONDS]
sccp answer CALL_ID [DEVICE]
sccp end CALL_ID
sccp originate DEVICE NUMBER [LINE] [ASSIGNED_CHANNEL_ID]
sccp set forwarding DEVICE LINE all|busy|noanswer DESTINATION|off
```

CLI bounds/defaults: message <=96 bytes, beep default `no`, ordinary message
timeout default 10 seconds, system-message timeout default 0 (persistent),
timeout `0..255`; device <=15 alphanumerics; line <=24 bytes; originate
destination <=79 bytes; assigned channel ID <=149 bytes with no whitespace;
forwarding destination <=23 bytes; call IDs are positive `u64`. `reset` and
`restart` send distinct SCCP handset reset modes. `all` targets registered
devices for reset/restart/message.

## Standalone `bridge` binary

This is separate from `chan_sccp2` and does not read `sccp.conf`. Run
`bridge [CONFIG_PATH]`; the positional path defaults to `bridge.toml`. Its TOML
rejects unknown fields.

| TOML key | Default / constraint |
| --- | --- |
| `sccp.bind` | `0.0.0.0:2000`; IPv4 socket only. |
| `sccp.keepalive_seconds` | `30`; >=5. Also used as secondary keepalive. |
| `sccp.server_name` | `sccp-protocol`. |
| `sccp.firmware_version` | empty; empty keeps installed handset firmware. |
| `sip.bind` | `0.0.0.0:5060`; IPv4 socket only. |
| `sip.advertised_address` | Optional IPv4 address; cannot be `0.0.0.0`. Omitted passes no explicit advertised address to the SIP stack. |
| `sip.conference_feature_code` | Optional string sent as SIP DTMF when the handset conference action is pressed; omitted disables that action. |
| `sip.interdigit_timeout_ms` | `3000`; >=250. |
| `media.bind_address` | Required IPv4 address on which RTP sockets bind. |
| `media.advertised_address` | Required nonzero IPv4 address advertised to phones/SIP peers. |
| `media.port_range` | Required string `START-END`; `u16`, inclusive, start <= end, even start, >=8 ports. |
| `media.direct_routes` | Optional array; empty keeps RTP relayed. Each route requires nonempty `phones` and `sip` IPv4-CIDR arrays. Direct RTP is eligible only when both endpoints match opposite sides of one route. |
| `phones[].device` | Required unique device ID: 1..15 ASCII alphanumerics, canonicalized uppercase. |
| `phones[].description` | Empty -> device ID; otherwise <=39 bytes, no controls. |
| `phones[].lines` | Required 1..6 entries; declaration order becomes line instances. |
| `phones[].lines[].number` | Required SCCP/SIP line number, 1..24 bytes. |
| `phones[].lines[].display_name` | Empty -> number. |
| `phones[].lines[].registrar` | Required `sip:` or `sips:` URI. |
| `phones[].lines[].outbound_proxy` | Optional SIP proxy URI. |
| `phones[].lines[].username` / `.password` | Required, nonempty SIP credentials. |
| `phones[].lines[].auth_username` | Optional authentication username. |

Complete TOML shape: `[sccp]`, `[sip]`, `[media]`, zero or more
`[[media.direct_routes]]`, one or more `[[phones]]`, and 1..6 nested
`[[phones.lines]]` per phone. See
[`bridge/config.example.toml`](../bridge/config.example.toml). The bridge has no
runtime-setting CLI; edit TOML and restart it. `RUST_LOG` controls logging but
is not SCCP configuration.
