# DND scheduling

A device can enter and leave Do Not Disturb automatically on a recurring
weekly schedule. Schedules belong to the configured device, so they continue
to apply when the phone is temporarily offline. The schedule runs in the
module, so Cisco 7961, 7962, and 7965 devices use the same settings.

## Configuration

Add one or more `dnd_schedule` entries to a device or device template:

```ini
[SEP00A1B2C3D4E5]
type = device
dnd_feature = yes
dnd = off
; Every night, rejecting new calls while the window is active:
; dnd_schedule = 22:00-07:00, *, reject
; Different weekday and weekend windows:
; dnd_schedule = 22:00-07:00, mon-thu, reject
; dnd_schedule = 23:00-09:00, fri-sun, silent
```

The entries above are commented deliberately. Uncomment only the policy that
should affect the phone.

The exact format is:

```text
dnd_schedule = HH:MM-HH:MM, days, silent|reject
```

- Times use the 24-hour clock with minute precision. A start time is
  inclusive and an end time is exclusive. `24:00` is permitted only as an end
  time; equal start and end times are invalid.
- `days` is `*` for every day, a day from `mon` through `sun`, a range such as
  `mon-fri`, or an `&`-separated combination such as `mon-wed&fri`. Day ranges
  may wrap across the end of the week, as in `fri-mon`.
- An overnight window such as `22:00-07:00, fri, reject` starts on Friday and
  ends at 07:00 Saturday. In other words, its day is the day on which it
  starts.
- `silent` suppresses new-call alerting. `reject` rejects new calls as busy.
  Existing active or ringing calls are not changed when a window begins.
- Each entry is limited to 128 bytes, and a device may have at most 32 windows.
  Duplicate or overlapping windows are rejected after their full weekly,
  including overnight, expansion. Adjacent windows are allowed.

Scheduling uses the Asterisk process's local timezone and follows its clock,
including daylight-saving changes. The SCCP `tzoffset` display setting does
not change schedule evaluation.

`dnd_feature` controls manual DND buttons and soft keys; it does not disable an
administrator-configured schedule. A scheduled transition still applies when
`dnd_feature = no`.

At module startup and after an effective schedule edit, the current window is
applied immediately. Thereafter the scheduler changes DND only when the
scheduled phase changes. A manual DND change made during a window therefore
remains in effect until the next schedule boundary. Reconnecting a phone or
reloading unrelated configuration does not overwrite that manual choice.

Device-template inheritance treats the schedule as a list. If a child device
contains any `dnd_schedule` entries, that list replaces its inherited list.
Use a sole `dnd_schedule = none` entry to clear an inherited schedule.

In Sorcery mode, schedules are ordered device fields. Use stable numeric
suffixes so multiple windows remain distinct:

```json
{"fields":[
  {"attribute":"dnd_schedule.0001","value":"22:00-07:00, mon-thu, reject"},
  {"attribute":"dnd_schedule.0002","value":"23:00-09:00, fri-sun, silent"}
]}
```

Validate changes before reloading them:

```sh
chan-sccp2-config-checker --canonical /etc/asterisk/sccp.conf
asterisk -rx 'sccp reload'
```

## Asterisk CLI overrides

The CLI can inspect or replace the effective schedule for any configured
device, whether or not it is currently registered:

```text
sccp dnd schedule SEP00A1B2C3D4E5 show
sccp dnd schedule SEP00A1B2C3D4E5 add 22:00-07:00 mon-thu reject
sccp dnd schedule SEP00A1B2C3D4E5 remove 1
sccp dnd schedule SEP00A1B2C3D4E5 clear
sccp dnd schedule SEP00A1B2C3D4E5 reset
```

`show` reports whether the rules come from configuration or a CLI override,
lists them with one-based indices, and displays both the current scheduled
phase and the phone's actual DND state.

CLI overrides are persisted in Asterisk's internal database. The first `add`
or `remove` copies the device's resolved configuration rules and edits that
copy. `clear` stores an explicit empty override. While an override exists,
later `sccp.conf` schedule changes are masked for that device. `reset` removes
the override and returns the device to its currently resolved configuration
schedule. An invalid edit, including one that creates an overlap, is rejected
without replacing the working schedule.
