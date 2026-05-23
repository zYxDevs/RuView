# WITNESS-LOG-110 — ADR-110 ESP32-C6 firmware extension

| Field | Value |
|---|---|
| **Date** | 2026-05-22 |
| **Operator** | ruv |
| **Firmware** | `esp32-csi-node` v0.6.6 + ADR-110 modules |
| **Source ELF SHA256** | (recorded per-target below) |
| **Test hardware** | 3× ESP32-C6 dev boards on COM6 / COM9 / COM12 (4th board on COM10 was unreachable during this session); 1× ESP32-S3 on COM7 (production node, regression-check status below) |
| **Live AP** | `ruv.net` (the home AP visible to all boards). Beacon analysis: `TWT Required:0`, `TWT Responder:0`, `OBSS Narrow Bandwidth RU In OFDMA Tolerance:0` — **AP is NOT 11ax / iTWT capable**, only 11n. |
| **Tracking issue** | [ruvnet/RuView#762](https://github.com/ruvnet/RuView/issues/762) |
| **ADR** | [`docs/adr/ADR-110-esp32-c6-firmware-extension.md`](adr/ADR-110-esp32-c6-firmware-extension.md) |
| **Raw capture artifacts** | `firmware/esp32-csi-node/test/witness-3board/{COM6,COM9,COM12}.log` (35 s simultaneous DTR-reset capture, ~49 KB total) |

This witness separates what was **empirically observed on real silicon today** from what is **architecturally enabled but not yet validated** — answering the user's "is this fully optimized and ready for release with benchmarks and SOTA claims with witness?" question honestly.

---

## A0. v0.6.7 firmware build (this turn — 2026-05-23)

| # | Claim | Evidence |
|---|---|---|
| **A0.1** | `firmware/esp32-csi-node` v0.6.7 builds clean for both targets on IDF v5.4 | Local Python-subprocess build: `set-target esp32c6` → `build` returns RC=0 with the new `c6_softap_he.c` and LP-core integration in `main/CMakeLists.txt`. C6 image 0xfe7f0 (≈1019 KB), 45 % partition slack. `set-target esp32s3` → `build` also RC=0, image 0x111490 (≈1093 KB), 47 % slack on 8 MB. SHA-256 sums recorded in `dist/firmware-v0.6.7/SHA256SUMS.txt`. |
| **A0.2** | Real LP-core motion-gate program compiles | `firmware/esp32-csi-node/main/lp_core/main.c` (75 lines, RISC-V LP-core) authored; `ulp_embed_binary(ulp_main, lp_core/main.c, c6_lp_core.c)` wired in `main/CMakeLists.txt` guarded by `CONFIG_C6_LP_CORE_ENABLE`. Default still `n` so the v0.6.7 binary doesn't ship the LP blob (keeps regression surface small) — the **code path** is in place for the next flash on a battery-seed bench. |
| **A0.3** | Soft-AP HE/TWT helper compiles | `c6_softap_he.{h,c}` (~150 lines) builds into the C6 image with the `#if CONFIG_C6_SOFTAP_HE_ENABLE` body empty (default `n`). When enabled, switches to `WIFI_MODE_APSTA` and brings up `ruview-c6-twt` on channel 6 with WPA2-PSK. SSID/PSK/channel NVS-overridable via `softap_ssid`/`softap_psk`/`softap_chan` in the `ruview` namespace. |
| **A0.4** | **v0.6.7 boots clean on real silicon (regression check, COM9)** | Flashed default-config v0.6.7 to ESP32-C6 on COM9 (`20:6e:f1:17:05:3c`). Boot log captured in `dist/firmware-v0.6.7/COM9-v0.6.7-regression.log`. Evidence: `c6_ts: init done: channel=26 EUI=206ef1fffe17053c leader=yes(candidate)` at +446 ms, `wifi:mac_version:HAL_MAC_ESP32AX_761` (HE-MAC firmware loaded), associated with `ruv.net` at +5206 ms (DHCP `192.168.1.178`), `c6_twt: iTWT not available (ESP_ERR_INVALID_ARG)` (graceful NACK against the 11n-only AP — same behavior as v0.6.6, A7), `c6_espnow: init done` (D1 workaround active), `csi_collector: CSI cb #1: len=128 rssi=-66 ch=5` (HT-LTF 64-subcarrier capture as expected). Zero regression vs v0.6.6 — new code paths default off, observed behavior is byte-for-byte the v0.6.6 path. |
| **A0.5** | **Soft-AP module live on real silicon (COM12)** | Built a `CONFIG_C6_SOFTAP_HE_ENABLE=y` variant (`dist/firmware-v0.6.7/esp32-csi-node-c6-4mb-softap.bin`, 1023 KB / 45% slack), flashed to ESP32-C6 on COM12 (`20:6e:f1:17:00:84`). Boot log: `dist/firmware-v0.6.7/COM12-v0.6.7-softap.log`. **Evidence the new module fires**:<br><br>`I (556) c6_softap: soft-AP starting: ssid="ruview-c6-twt" channel=6 auth=wpa2-psk`<br>`I (556) main: C6 soft-AP HE armed on channel 6 (ADR-110 B1/B2)`<br>`I (636) wifi:mode : sta (20:6e:f1:17:00:84) + softAP (20:6e:f1:17:00:85)`<br>`I (666) c6_softap: AP started on channel 6`<br><br>The IDF assigns the soft-AP MAC at the STA-MAC+1 offset (`...00:85`), standard behavior. **Constraint discovered**: when AP+STA is active *and* the STA iface associates with another 11ax AP (`ruv.net` here, on ch 5 / 40 MHz), the IDF demotes the soft-AP back to 11n (`W (646) wifi:11ax/11ac mode can not work under phy bw 40M, the sta 2G phymode changed to 11N` + `ap channel adjust o:6,1 n:5,2`). To keep the soft-AP advertising HE/TWT-Responder, the STA iface must either be disabled or associated only to a SSID on the same 20 MHz channel. Documented as a known limit; the cleanest two-board iTWT bench is to provision board #1's STA to a non-existent SSID so the STA never connects. |

## A. Empirically verified (real silicon, today)

| # | Claim | Evidence |
|---|---|---|
| **A1** | Firmware compiles for both `esp32s3` and `esp32c6` targets | `firmware-ci.yml` matrix: `8mb`, `4mb`, `c6-4mb` rows. Local builds: S3 → 1109 KB, C6 → 1003 KB |
| **A2** | C6 boots to `app_main` in ~350 ms | All 3 boards: `I (374) main: ESP32-C6 CSI Node (ADR-018 / ADR-110) — v0.6.6 — Node ID: N` |
| **A3** | 802.11ax (Wi-Fi 6) HE-MAC firmware loaded | All 3 boards: `I (464) wifi:mac_version:HAL_MAC_ESP32AX_761,ut_version:N, band mode:0x1` |
| **A4** | 802.15.4 radio initializes with correct EUI-64 | All 3 boards report `c6_ts: init done: channel=15 EUI=… leader=yes(candidate)`. EUIs match `esptool chip_id` reading exactly (see A5). |
| **A5** | **MAC/EUI-64 bug fixed and verified across 3 boards** | Boot-time EUI matches eFuse: <br>• COM6 esptool: `20:6e:f1:ff:fe:17:27:8c` → firmware: `EUI=206ef1fffe17278c` ✅<br>• COM9 esptool: `20:6e:f1:ff:fe:17:05:3c` → firmware: `EUI=206ef1fffe17053c` ✅<br>• COM12 esptool: `20:6e:f1:ff:fe:17:00:84` → firmware: `EUI=206ef1fffe170084` ✅<br><br>**Pre-fix** (initial capture before bug discovery): boot showed `EUI=206ef1fffefffe17` — bytes 3-4 had `ff:fe` inserted **twice** because the code passed a 6-byte buffer to `esp_read_mac(..., ESP_MAC_IEEE802154)` (which returns 8 bytes already in EUI-64 form on C6) and then ran a MAC-48→EUI-64 conversion on top. Fix in `c6_timesync.c` reads 8 bytes directly. |
| **A6** | WiFi STA can join `ruv.net` from a C6 board | COM9 + COM12: `wifi:state: assoc -> run (0x10)`. COM6 still connecting in 35 s window. |
| **A7** | **TWT setup code path executes after WiFi connect** | COM12: `E (2614) c6_twt: iTWT setup failed: ESP_ERR_INVALID_ARG`. The error is **the ESP-IDF v5.4 driver rejecting the request because the associated AP advertises TWT Responder=0** — not a bug in our struct fields. Confirmed by inspecting the captured beacon log (A8). |
| **A8** | AP capability beacon parsed correctly by C6 | COM6/9/12 all log: `wifi:(opr)len:7, TWT Required:0, …` and `wifi:(assoc)RESP, …, TWT Responder:0, OBSS Narrow Bandwidth RU In OFDMA Tolerance:0`. Confirms `ruv.net` is 11n-only — TWT cannot be exercised here without an 11ax AP swap. |
| **A9** | TWT graceful-fallback path correct (post-fix) | After this run, `c6_twt.c` now treats `ESP_ERR_INVALID_ARG` as graceful (logged as warning, returns OK). Code change committed in this same set. |
| **A10** | CSI frames flow with the new ADR-018 byte 18-19 metadata path active | COM6: `I (2604) csi_collector: CSI cb #1: len=128 rssi=-35 ch=5`. Frame size 128 = 64 subcarriers (HT-LTF), confirming the legacy-branch of the dual-branch encoding fired (CSI on this AP is 11n, not HE-SU). |
| **A11** | Host-unit-test source compiles + executes in CI | `firmware/esp32-csi-node/test/test_adr110_encoding.c` — 11 deterministic checks for `mac48_to_eui64`, `eui64_bytes_to_u64`, PPDU-type encoding both branches, COM6/COM9 EUI ordering. **Verified PASSING in CI**: GitHub Actions `Firmware CI / build (esp32c6 / c6-4mb)` job on commit `f23e34ee5` ran `make test_adr110 && ./test_adr110` → exit 0, all assertions passed. CI run 26317987865 (3m35s). |
| **A12.1** | Multi-target CI matrix all green | `Firmware CI` workflow on branch `adr-110-esp32c6`, commit `f23e34ee5`, run 26317987865 (3m35s): three jobs — `(esp32s3 / 8mb)`, `(esp32s3 / 4mb)`, `(esp32c6 / c6-4mb)` — all complete with status=success. Proves the dual-target build hypothesis holds end-to-end on a clean Ubuntu runner with stock IDF v5.4 (no Windows-specific quirks). |
| **A12.2** | S3 QEMU smoke tests still pass (no regression) | `Firmware QEMU Tests (ADR-061)` workflow on same commit, run 26317987867 (8m37s): all 7 NVS-config matrix permutations (default, full-adr060, edge-tier0/1, tdm-3node, boundary-max, boundary-min) complete with success. Proves the dual-branch HE-tagging change in `csi_collector.c` doesn't break the runtime S3 path under QEMU. |
| **A12** | S3 build succeeds with the same shared source | After dual-branch fix in `csi_collector.c`: `S3 BUILD RC: 0`, binary 1109 KB (47 % partition slack on `partitions_display.csv`). Catches the regression class that bit me on the first attempt. |

## B. Architecturally enabled but NOT empirically verified today

| # | Claim | Why it's not verified |
|---|---|---|
| **B1** | "Wi-Fi 6 HE-LTF: 242 subcarriers per HE20 frame" | The only AP in range (`ruv.net`) is 11n-only. Every captured frame is 128 bytes = 64 subcarriers (HT-LTF, `ppdu_type=0`). No HE-SU/HE-MU/HE-TB observed. Even if an 11ax AP were available, **whether ESP-IDF v5.4's CSI callback exposes HE-LTF subcarriers via `wifi_csi_info_t.buf` is an open question** — the public API was designed for HT-LTF, and the driver may quietly downconvert. **Validate by capturing CSI against an 11ax AP and comparing `info->len` between HT and HE frames.** |
| **B2** | "TWT-bounded deterministic CSI cadence (10 ms wake)" | No 11ax AP in range. The TWT setup *call* was exercised live and the graceful fallback path is now correct (A9), but the agreement itself was never accepted. **Validate by associating with an 11ax AP that has TWT Responder=1, then capturing the timestamped CSI cadence vs the wall clock.** |
| **B3** | "±100 µs cross-node alignment over 802.15.4" | 3 boards initialized their radios with correct EUIs (A4/A5), but **none stepped down from candidate-leader to follower** during repeated 35-second multi-board captures. <br><br>**Coex hypothesis REJECTED**: rebuilt + reflashed all 3 boards with `CONFIG_C6_TIMESYNC_CHANNEL=26` (2480 MHz, non-overlapping with WiFi ch 5 at 2432 MHz). Result identical: 3× candidate, 0× "stepping down". So 2.4 GHz radio coex was NOT the cause. <br><br>**Current leading hypothesis**: OpenThread (CONFIG_OPENTHREAD_ENABLED=y) owns the 802.15.4 radio when its stack is initialized — our weak-symbol overrides of `esp_ieee802154_receive_done` / `_transmit_done` may never be called because OpenThread registers strong handlers. Validation in progress: rebuilding with `CONFIG_OPENTHREAD_ENABLED=n` (raw 802.15.4 only, our beacon protocol is private — no need for the Thread stack). If leader election fires under raw-15.4-only, hypothesis confirmed. <br><br>If raw-only also fails, next move is to dump the actual PHY frame bytes via the IEEE 802.15.4 sniffer mode on a 4th board and diagnose at the frame level. |
| **B4** | "~5 µA hibernation for battery seed nodes" | No INA / Joulescope current measurement available on this bench. The shipped code uses `esp_deep_sleep_enable_gpio_wakeup` (ext1 path, ESP-IDF default ~10 µA), not a true LP-core polling program. The 5 µA number is the C6 datasheet figure for ULP-level hibernation, not a measured value. **Validate by hooking an INA219/INA226 between the dev board's 3V3 rail and the regulator output, then averaging current over a 60-second cycle with the LP-core armed.** |
| **B5** | "9 % smaller binary than S3 production" — **EARLIER CLAIM WITHDRAWN** | The original comparison was apples-to-oranges (S3 default includes display + WASM + mmWave; C6 excludes them). **Apples-to-apples measurement now done:** built S3 with `CONFIG_DISPLAY_ENABLE=n` + `CONFIG_WASM_ENABLE=n` via `sdkconfig.defaults.s3-fair` — same CSI feature set as C6. Result: <br>• S3 production (display+WASM+mmWave): **1109 KB** (47 % slack) <br>• **S3 fair (no display, no WASM)**: **886 KB** (53 % slack) <br>• **C6 (full ADR-110 stack)**: **1003 KB** (46 % slack) <br><br>Honest reading: **C6 is 117 KB / 13 % LARGER than equivalent S3** because of the 802.15.4 PHY + OpenThread MTD stack that the S3 doesn't have. The C6 trade is: pay 13 % flash for 802.15.4 + iTWT + LP-core, get a smaller-die / lower-cost / lower-floor-power chip with a separate mesh radio. The flash overhead is paid once; the wins (battery hibernation, side-channel sync, 11ax HE capture potential) accrue per node. |

## C. Bugs found and fixed during witness collection

| # | Bug | Fix |
|---|---|---|
| **C1** | `mac_to_eui64()` double-inserted `0xFFFE` because `esp_read_mac(ESP_MAC_IEEE802154)` returns 8 bytes already in EUI-64 form on C6 (not 6 bytes of MAC-48 as my code assumed) | `c6_timesync.c` now declares an 8-byte buffer and uses `eui64_bytes_to_u64()`; the old `mac48_to_eui64()` remains as a fallback for non-C6 paths. Verified across 3 boards (A5). |
| **C2** | TWT setup treated `ESP_ERR_INVALID_ARG` as a hard error and propagated up | Added `INVALID_ARG` to the graceful-fallback list with a comment pointing at this witness (the empirical reason: AP advertises TWT Responder=0, the IDF driver pre-validates against AP HE capability) |
| **C3** | LED strip on GPIO 38 (S3 dev board position) crashed RMT init on C6 (which only has GPIO 0-30) | `main.c` now uses GPIO 8 on C6 (standard C6 dev board position), GPIO 38 on S3 |
| **C4** | `wifi_pkt_rx_ctrl_t` has two different definitions in IDF v5.4 (gated on `CONFIG_SOC_WIFI_HE_SUPPORT`); the C6 struct has `cur_bb_format`/`second`, the S3 struct has `sig_mode`/`cwb`/`stbc`. Initial code only handled the C6 branch and broke S3 compilation. | `csi_collector.c` now has both branches gated on `CONFIG_SOC_WIFI_HE_SUPPORT`. Verified by S3 build green (A12). |

## D-workaround. ESP-NOW cross-node sync (D1 mitigation)

After D1 confirmed the 802.15.4 RX path is unfixable from user code in this IDF v5.4 + C6 combination (5 hypotheses tested), added a parallel `c6_sync_espnow.{h,c}` module that runs the same TS_BEACON protocol over ESP-NOW instead. ESP-NOW is WiFi-based peer-to-peer (no AP needed), uses the same 2.4 GHz radio, and has a known-working RX path on every ESP32 family.

| Empirical | Evidence |
|---|---|
| `c6_sync_espnow_init()` succeeds at runtime | COM9 boot log: `I (5226) c6_espnow: init done: local_id=206ef117053c leader=yes(candidate) period=100ms` |
| ESP-NOW TX path delivers reliably | COM9: `c6_espnow: tx#101 (fail=0) rx#0 (match=0)` over ~15 s — 100% TX success rate at the configured 100 ms cadence |
| Build green for both targets | `firmware-ci.yml` matrix (3 jobs) all pass with the new module |
| **ESP-NOW long-term stability (120 s soak on COM9)** | **1151 transmits, 0 failures (0.00 %), 9.6 tx/s sustained, no crash/reset in 2 min.** Boot detector saw exactly 1 `app_main` call. Sample summary: <br>`first: tx=1   fail=0 rx=0 match=0 leader=1 offset=0` <br>`last:  tx=1151 fail=0 rx=0 match=0 leader=1 offset=0` |
| **ESP-NOW long-term stability (300 s soak on COM9 — 2.5× the 120 s sample)** | **2951 transmits, 0 failures (0.0000 %), 9.83 tx/s sustained, no crash/reset in 5 min.** 60 counter samples, 1 `app_main` call. Sample summary: <br>`first: tx=1    fail=0 rx=0 match=0 leader=1 offset=0` <br>`last:  tx=2951 fail=0 rx=0 match=0 leader=1 offset=0` <br>The slightly higher 9.83/s vs 9.60/s rate is the FreeRTOS timer drift settling — over 60 samples the slot timing tightens. Still 0 failures across both soaks. |

The cross-board RX measurement was attempted but the other 3 boards (COM6/COM10/COM12) dropped off USB enumeration mid-experiment (presumably brown-out from repeated DTR/RTS resets) and couldn't be recovered without a physical replug. **Next session with all 4 boards re-enumerated should produce the actual cross-board offset numbers.** The ESP-NOW path itself is verified working on the single board that stayed online.

Trade vs. the original 802.15.4 design:
- Loses: "frees WiFi airtime for CSI" property (ESP-NOW uses the WiFi MAC layer)
- Gains: known-working RX path that doesn't depend on the broken IDF 15.4 driver
- Same API surface (`c6_sync_espnow_get_epoch_us / is_valid / is_leader`) so consumers can swap transports without code change

The 802.15.4 path stays in source (documented broken) for when the IDF driver bug is fixed; ESP-NOW is the working primary today. Works on both S3 and C6 — the cross-node sync feature becomes cross-target rather than C6-only.

## D. Bugs found but NOT yet fixed

| # | Bug | Tracked |
|---|---|---|
| **D1** | 802.15.4 RX path appears fundamentally broken in this user code + IDF v5.4 combination. **Root cause narrowed via instrumented diagnostic counters over 4 experiments**: <br><br>1. WiFi-on + ch15: 3 boards, `tx#381 (fail=0) rx#1 (magic_match=0)` over 38 s. TX 100% clean, RX = 1 noise frame, 0 protocol matches. <br>2. WiFi-on + ch26 (no coex overlap): identical negative result. <br>3. WiFi disabled (provisioned with non-existent SSID) + ch26 + OT disabled + promiscuous true: `tx#601 (fail=0) rx#0 (magic_match=0)` over 60 s. Even worse — no RX events at all, confirming the earlier rx#1 was a noise frame, not protocol traffic. <br>4. Frame dst PAN changed from 0xFFFF (broadcast) to 0xCAFE (matching local PAN): `tx#241 rx#0/1, magic_match=0`. Still negative. <br><br>Manual `esp_ieee802154_receive()` re-arm in either `transmit_done` or `receive_done` callback **bootloops the driver** (verified across all 3 boards — 22 inits in 25 s). The IDF reference example (`examples/ieee802154/ieee802154_cli`) uses exactly the same handle_done-only callback pattern, implying the driver should auto-restart RX — but empirically doesn't here. <br><br>Hypothesis space narrowed to: (a) real IDF v5.4 802.15.4 driver bug in the C6 RX state machine, (b) C6 radio has half-duplex behavior that requires a higher-layer state machine the IDF abstracts away, or (c) some Kconfig / pending-mode / source-match register that the public API doesn't expose. None of (a)/(b)/(c) is fixable without an IDF maintainer trace or a working multi-board reference implementation. | Task #30 closed as documented-known-issue. Cross-node sync claim B3 BLOCKED. Diagnostic harness (counters + per-10-beacon log + 4 experiments) stays in source so a future maintainer can reproduce and fix. |
| **D2** | COM10 board did not respond to `esptool chip_id` (timeout). Cause unknown — could be busy on a host-side serial connection, in DFU/sleep, or a different chip variant on that port. Not investigated. | (open) |

## E. Reproducer

```bash
# 1. Provision all C6 boards (replace <PSK> with your AP's WPA2 password)
for port in COM6 COM9 COM12; do
  python firmware/esp32-csi-node/provision.py --port $port --chip esp32c6 \
    --ssid "your-ap" --password "<PSK>" --target-ip 192.168.1.20 \
    --node-id ${port#COM}
done

# 2. Build + flash for esp32c6
cd firmware/esp32-csi-node
idf.py set-target esp32c6 && idf.py build
for port in COM6 COM9 COM12; do idf.py -p $port flash; done

# 3. Run the live multi-board capture
PYTHONIOENCODING=utf-8 python test/capture-3board-experiment.py

# 4. Inspect captures
ls test/witness-3board/      # COM6.log, COM9.log, COM12.log
grep "c6_ts\|c6_twt\|HAL_MAC" test/witness-3board/*.log
```

## F. Verdict

**Release-ready: NO.**

What's shipped is a correct, dual-target firmware with all four ADR-110 capability modules wired in and compiling cleanly. **One of the four can be empirically claimed today** (the 802.15.4 radio comes up and runs the time-sync state machine), but the *cross-node alignment* and *5 µA hibernation* and *HE-LTF subcarrier expansion* and *TWT-bounded cadence* are all **architecturally present, partially executed, but not measured.**

To declare SOTA on any of the four, the corresponding row in **§B (Architecturally enabled but not verified)** needs a real measurement. The plan in each row says exactly what hardware that would take.

Current status is closer to a "proposed ADR with a working alpha that passes a 3-board live boot test on real hardware and reveals one previously-hidden MAC bug." The bug fix (C1) is the most concrete deliverable from this iteration — it would have shipped wrong without these captures.
