# sccp-rs -> Cisco SCCP implemented in Rust

This is repo contains three things

- Implementation of the Cisco SCCP protocol in Rust
- Asterisk module for SCCP *(chan_sccp2)* written in Rust
- Simple SCCP<->SIP app to bridge an SCCP phone to SIP

I have some Cisco phones where the SIP firmware is truly ass to tinker with for a variety of reasons. Most people on the internet recommends running these phones on the SCCP firmware rather than the SIP firmware. This however poses the issue of what server one should use to drive the SCCP firmware. You either have to use [Cisco CallManager](https://www.webex.com/us/en/products/suite/enterprise-cloud-calling/CUCM.html), a plugin to Asterisk called [chan_sccp](https://github.com/chan-sccp/chan-sccp). The first one is way outside of my price range and the second one is somewhat abandoned, only functioning on older version of Asterisk (some users reporting having to run this on Asterisk 19). There are some other projects but they are either a full PBX or has some commercial angle to it.

Given I was pursing a project that was going to use SCCP phones, I was somewhat motivated to solve this problem. After working with [PJSIP](https://www.pjsip.org/) for [SIPcord](https://sipcord.net/), I came to really appreciate the way they built a self contained library that implemented the core of what you needed to do SIP. `chan_sccp` implements the protocol but it's very much tied to implementation details of Asterisk.

This project shoots in the same direction as PJSIP by separating the **protocol** and the **asterisk module** into two distinct crates. The protocol crate can easily be reused for other implementations, which is what I'm doing with my small app *(like pjsua)* but I also wanted to validate the implemenation, hence the Asterisk module. It was developed by researching the protocol from [chan_sccp](https://github.com/chan-sccp/chan-sccp) and [mod_skinny](https://github.com/sangoma/freeswitch/tree/master/src/mod/endpoints/mod_skinny), both of which we all owe a tremendous deal for laying the groundwork for making something like this possible.

**But why Rust?** Apart from my short stint with C++ earlier in my career, most of my "compiled" development has been in Go and Rust, for the last 6 years it has been Rust. I've done so much C/C++ interop stuff with Rust that it's the natural choice for me. This is is no way a reflection or comment on language choice, projects like Asterisk has stood the test of time and is written in C. It's more a reflection on myself. I don't think I am big brained enough to produce this in C and I was certainly not smart enough to make `chan_sccp` work for Asterisk 23. Given that Rust can interop with C without much trouble, I wrote the Asterisk module in pure Rust with `bindgen` producing the ABI needed to interop.

## AI DISCLAIMER

I heavily used `gpt-5.6-sol`. This was **NOT** a *"make no mistakes, one shot"* ordeal, but rather a very tedious multi-week process with me reviewing protocol implementations and iterating with the model. I'm being as upfront about this as I can, you can spare me the purity testing.

## SCCP Protocol (sccp-protocol)

The [sccp-protocol](https://crates.io/crates/sccp-protocol) crate provides a typed SCCP wire codec and an asynchronous [Tokio](https://tokio.rs/) server for building call-control applications around Cisco SCCP phones. It handles message framing and serialization, registration and session lifecycle, phone provisioning, call and media control, soft keys, QoS, and Cisco IP Phone XML. Applications receive strongly typed events from connected phones and send typed commands through a cloneable server handle.

The crate deliberately contains no SIP or PBX policy. It can be used as the phone-facing layer of an Asterisk channel driver, a protocol analyzer, or a standalone SCCP application such as the bridge in this repository. See the [API documentation](https://docs.rs/sccp-protocol) for a server example and the lower-level message APIs.

## Asterisk Module

In `asterisk-module` you will find a completely new channel driver called `chan_sccp2`, written in Rust. The current production module targets the Asterisk 22+ ABI generation: releases are built against Asterisk 22, while source compatibility is continuously tested against upstream Asterisk master.

The driver exposes SCCP as a native Asterisk channel and connects the protocol server to Asterisk's dialplan, RTP/media, device state, hints, message waiting, parking, pickup, transfers, conferencing, call forwarding, CLI, AMI, realtime configuration, and with [sorcery ARI configuration](docs/DYNAMIC_CONFIGURATION.md). It is a new **completely new** implementation built on `sccp-protocol`, not a fork of the old `chan_sccp` module.

You can find the latest [pre-compiled release](https://github.com/coral/sccp-rs/releases) for Linux x86-64 and ARM64/aarch64. This includes 64-bit Raspberry Pi 4 and 5 systems. If you'd rather build the x86-64 module locally, the easiest path is Docker.

```sh
# For Asterisk 22
./asterisk-module/build-linux-x86_64.sh 22
# or for current upstream Asterisk master
./asterisk-module/build-linux-x86_64.sh latest
```

The local artifact is written to `dist/chan_sccp2-asterisk-linux-x86_64-v<module-version>.so`. Published releases contain versioned normal and opt-in debug telemetry modules for both architectures. Install the selected artifact in Asterisk's module directory as `chan_sccp2.so`, copy `asterisk-module/sccp.conf.example` ( or [copy it from here](https://github.com/coral/sccp-rs/blob/master/asterisk-module/sccp.conf.example)) to Asterisk's configuration directory as `sccp.conf`, and edit the example device and line definitions for your phones.

*Why is there only one .so file? Doesn't Asterisk modules normally need to be compiled for each distribution and exact Asterisk build?*

Many Asterisk modules depend heavily on Asterisk internals and compile-time features, which ties their binaries to the distribution's particular Asterisk package and build options. `chan_sccp2` keeps that dependency behind a deliberately narrow native adapter. The SCCP wire protocol, Cisco IP Phone XML, TCP/TLS servers, runtime, thread pool, call state machines, and most feature policy are implemented independently in Rust rather than inherited from the host Asterisk build.

The remaining boundary is Asterisk's C ABI. Release modules deliberately opt out of Asterisk's exact build-option checksum and instead verify that the running major is at least their ABI baseline. If a future Asterisk major breaks this ABI generation, releases will add a new baseline artifact at that point. Release files are architecture-specific and require a glibc-based Linux system. If you happen to use another platform, compiling this is easy thanks to the tooling in Rust.

## Releasing

```sh
cargo release patch --execute
```

## SCCP<->SIP app

in progress

## Feature Coverage

### SCCP Protocol (`sccp-protocol`)

| Area | Implemented | Not implemented / out of scope |
| --- | --- | --- |
| Wire protocol | Checked framing and typed codecs for the SCCP/SPCP catalog; unknown payloads are bounded and preserved | — |
| Station sessions | Async TCP server, injected clear/TLS streams, registration, keepalives, token fallback, failover advertisement, and live reconfiguration | Built-in TLS certificate/listener management and network ACLs |
| Phone UI and services | Lines, buttons, soft keys, lamps, tones, prompts, MWI/BLF, call state, provisioning models, and typed Cisco IP Phone XML | TFTP/HTTP boot server, firmware distribution, and arbitrary proprietary XML schemas |
| Media signaling | Audio and video channel control, DTMF, multicast, statistics, announcements, conferencing, and QoS/RSVP messages | RTP transport, codecs/transcoding, recording, and conference mixing |
| Application policy | Typed event/command API for call-control applications | SIP, PBX, dialplan, routing, persistence, and authorization policy |

### Asterisk Module (`chan_sccp2`)

| Area | Supported | Not supported |
| --- | --- | --- |
| Platform | Asterisk 22+ on Linux x86-64 and ARM64/aarch64; tested at the Asterisk 22 baseline and against upstream master | You're kinda on your own with ARM32 and other stuff LOL |
| Signaling and network | SCCP over TCP or TLS, IPv4/IPv6, NAT address selection, DSCP/COS, registration failover, and per-device transport policy | Configured network/hostname ACL admission |
| Calling features | Inbound/outbound calls, hold, call waiting, auto-answer, shared lines, transfer, forwarding, pickup, park, barge, DND/privacy, voicemail/MWI, BLF/hints, mobility, call completion, [one-touch, armed, and AMI MixMonitor recording](docs/RECORDING.md), and conferencing | Phone firmware/TFTP provisioning and a built-in PBX or SIP stack |
| Audio media | Native RTP, early media, jitter buffer, direct RTP, DTMF, and mapped G.711/G.722/G.723/G.729/G.726, GSM, iLBC, Siren7, SLIN16, and Opus | Protected/SRTP media and SCCP codecs with no Asterisk format mapping |
| Video media | Anchored RTP and handset control for H.261, H.263/H.263+, and H.264, including fast-picture updates | Direct video RTP, H.265 stream setup, H.264 SVC/FEC/UC, and encrypted video |
| Configuration and control | File, realtime, or Sorcery/AstDB config; ARI dynamic objects; transactional reloads; device state/hints; durable background selection; CLI; AMI actions/events; dialplan functions/apps; and an HTTP phone directory | The phone-authentication HTTP route (API only; no credential backend is installed) |

## Developing

- Clone the repo
- `git submodule update --init --recursive`
- `cargo build`

## Contributing

PRs welcome

## License

All workspace packages are licensed under MIT except `asterisk-module`, which is
licensed under GPL-2.0-or-later. The full GPLv2 terms are included in
`asterisk-module/asterisk/COPYING`.
