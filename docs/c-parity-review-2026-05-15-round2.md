# C parity review — 2026-05-15 round 2

4개 서브에이전트 팀이 worktree 격리에서 병렬로 `crates/epics-base-rs`, `crates/asyn-rs`, `crates/epics-pva-rs`, `crates/epics-ca-rs`를 C/C++ 원본과 대조해 리뷰·수정·커밋한 결과. 라운드 1(`docs/c-parity-review-2026-05-15.md`)의 후속.

참조 소스:

- `~/codes/epics-base/modules/{database,libcom,dbStatic}/src/` — recGbl, dbAccess, macLib, iocsh
- `~/codes/epics-base/modules/ca/`, `~/codes/epics-base/src/cas/` — libca client, rsrv 서버, repeater
- `~/codes/epics-modules/asyn/asyn/` — asynDriver 프레임워크
- `~/codes/epics-modules/pvxs/src/` — PVAccess C++ 구현 (서버/클라이언트, 와이어, 모니터, RPC) — **신규 라운드**

방법론은 글로벌 룰 *Fixes from reported defects* + *Invariant-driven fixes* — 각 finding마다:

1. 결함의 구조적 anchor 식별
2. workspace-wide `rg`
3. 모든 hit을 `same defect (fix now)` / `distinct (one-line why)` 분류
4. same defect 모두 한 커밋으로 묶음 fix
5. nextest + clippy 그린 후 커밋

각 finding의 commit body가 audit trail이다. 이 문서는 요약.

## 라운드 결과

| 팀 | 크레이트 | 커밋 | 머지 | 테스트 |
|---|---|---|---|---|
| A | `epics-base-rs` | 4 | (FF on parent) | 1163 nextest (+6 회귀) |
| B | `asyn-rs` | 3 | `3f92b7e` | 411 nextest (+18 회귀) |
| C | `epics-pva-rs` | 5 | `caee07a` | 388 nextest (+9 회귀) — **신규** |
| D | `epics-ca-rs` | 5 | `c2aa1e6` | 188 nextest (+11 회귀) |
| 통합 | (병합 시 발견) | 2 | parent | — |

워크스페이스: `cargo nextest --workspace` **3573/3573 PASS** (32 skipped), `cargo test --doc --workspace` 0 fail, `cargo clippy --workspace --all-targets -- -D warnings` clean.

## 수정된 finding

### Team A — `epics-base-rs` ↔ `epics-base/modules/{database,libcom}/src/`

#### 1. `16e0ff6` — dbProcess entry-level PACT guard + AsyncPending sets pact

- **Anchor**: `instance.processing.store` / `RecordProcessResult::AsyncPending`
- **Invariant**: `process_record_with_links_inner`에 들어오는 모든 foreign caller(FLNK, scan, scan_event, CA put)는 `is_processing() == true`인 레코드에 대해 `record.process()`를 호출해선 안 된다. C `dbAccess.c::dbProcess:537-559`: pact=true면 lcnt++ 후 silent bail; MAX_LOCK=10 초과 시 SCAN_ALARM/INVALID + "Async in progress" + VAL monitor 발화.
- **Owner**: `process_record_with_links_inner` (single gate)
- **Why**: 기존 Rust 포트는 entry 가드 없이 `record.process()`를 바로 호출 → 비동기 device-support 중 FLNK/scan이 mid-cycle 재진입 → 상태 머신 부패 + OUT/FLNK dual-fire.
- **Side fix**: `AsyncPending` 분기에서 `processing.store(true)`가 빠져 있었음 (`process_local`만 swap, 메인 path는 빈 채 진입) — 이로 인해 entry 가드가 무의미해지는 문제도 동시에 해결.

테스트: 2 (`test_pact_entry_guard_silent_bail_until_max_lock`, `test_pact_entry_guard_resets_lcnt_after_completion`).

**라운드 후속**: 이 커밋이 `scaler-rs::test_scaler_dly_delayed_start` 회귀를 일으켰음 — `27e0bb0`에서 owner-driven continuation API로 분리 봉합. 아래 통합 섹션 참조.

#### 2. `6dc7293` — calc record analog alarm limits + AFTC filter

- **Anchor**: `RecordInstance::new_boxed`의 `match rtype { ... }`, `evaluate_analog_alarm`
- **Why**: calc/calcout이 `analog_alarm` 슬롯을 할당받지 못해 HIHI/HHSV/HIGH/HSV/LOW/LSV/LOLO/LLSV put이 silent no-op. C `calcRecord.c::checkAlarms:339-381`은 AFTC time-constant filter를 통해 hysteresis-style 알람 발화. `RecordInstance::new_boxed`에 `"calc"|"calcout"` 분기 추가, 정수 `alarm_range` 플러밍.

테스트: 2 (analog alarm limits, AFTC filter).

#### 3. `531ec4f` — substitute_macros backslash escape

- **Anchor**: `chars[i] == '$' && (chars[i + 1] == '(' || chars[i + 1] == '{')`
- **Why**: C `macCore.c:740-749` level-0 `\<char>` 분기 누락 → `$(a)\$(b)` 형태에서 `\$`가 그대로 macro expansion 트리거. db_loader 측의 `substitute_macros`에 `\` 이스케이프 추가 (preserves the `\` itself).

테스트: 1.

#### 4. `54fc7ac` — iocsh `N>` / `N>>` fd-numbered redirect

- **Anchor**: `b'>' if !in_quote` in `parse_redirect`
- **Why**: C iocsh는 `cmd 2> err`, `cmd 3>>log` 등 single-digit fd redirect 인식. Rust는 `>`, `>>`만 → site st.cmd가 stderr 분리 불가. parse_redirect에 token-boundary digit 감지 추가.

테스트: 1.

**Deferred** (cross-cutting / 다음 라운드):

- scan_event periodic_scan_loop 직렬 처리 (architecture change — scanOnce-style queueing 필요)
- iocsh `2>` true stderr capture (모든 `eprintln!` 호출처에 with_error API 필요)
- iocsh mid-line `<` redirect (SourceContext refactor 필요)
- TPRO "Active 'X' with RPRO=N" 진단 메시지 (관측성만, 영향 낮음)

### Team B — `asyn-rs` ↔ `epics-modules/asyn/`

#### 1. `ad575cf` — strict `get_*_strict` variants + setter defined-flip

- **Anchor**: `match &self.get_entry(.).?.value`
- **Why**: C asyn `paramVal::getInteger` 등은 미정의 시 `asynParamUndefined` 반환. Rust `ParamList::get_int32`는 `.unwrap_or(0)` lax → 캐싱-체이닝 회로가 미정의를 0으로 흡수. 새 surface `AsynError::ParamUndefined(usize)` + `get_int32_strict / get_int64_strict / get_float64_strict / get_string_strict / get_uint32_strict` 추가, 기존 lax는 보존(`ad-core-rs/ad-plugins-rs` 호환). 동시에 setter들이 `!isDefined() || (old != value)`로 defined를 flip하도록 수정 (`paramVal::setInteger/setDouble/setString/setUInt32` C parity).

테스트: 10.

#### 2. `242a4ce` — `TraceManager::output_device_*` + `asyn_trace_device!` 매크로

- **Anchor**: `fn output(_with_source|_io)?\b` / `configs.get(port)`
- **Why**: C `findTracePvt`/`findDpCommon` (asynManager.c:530-549/3038-3099)는 device → port → global trace config 계층을 walk. Rust `TraceManager::output*`은 port→global만 참조 → device-override가 출력에 미반영. `with_effective_config(port, addr, f)` resolver 추가, `output_device / output_device_with_source / output_device_io` API + `asyn_trace_device! / asyn_trace_device_io!` 매크로 (`is_enabled_device` 게이트). `format_prefix_addr`이 `port:addr` 임베드 (C `printPort` parity).

테스트: 6.

#### 3. `0b9e8d5` — `InterruptValue.aux_status` + `alarm_status/Severity`

- **Anchor**: `call_param_callbacks` / `call_param_callback`
- **Why**: C `asynPortDriver.cpp:631-642` 등 interrupt callback에 `pasynUser->auxStatus`, `alarmStatus`, `alarmSeverity` 운반. Rust `InterruptValue`는 value만 운반 → asynRecord SEVR/STAT plumb 안 됨. `InterruptValue`에 3 필드 추가, `get_param_status`로 채움. `Default` impl 추가로 ~20개 테스트 construction에 `..Default::default()` cascade 흡수. 1 prod cascade (`ad-core-rs/src/plugin/runtime.rs:498`).

테스트: 2.

**Deferred**:

- `create_param` strict `asynParamAlreadyExists` — ad-core-rs `ADDriverParams::create`가 idempotent silent-dedup에 의존하여 cascade 11 fail. 별도 라운드 필요.
- asynOctet `eom_reason` 미전파 (전 driver surface 변경, 라운드 범위 넓음).
- asynUInt32Digital interrupt mask API (`uInt32RisingMask/FallingMask` + `setInterrupt/clearInterrupt/getInterrupt`) — feature gap, regression 아님.

### Team C — `epics-pva-rs` ↔ `epics-modules/pvxs/src/` (신규)

#### 1. `3bea633` — SEARCH MustReply flag + empty SEARCH_RESPONSE `found=0`

- **Anchor**: SEARCH handler in `server_native/udp.rs`
- **Why**: pvxs `server.cpp:730-744`: MustReply flag(`bit 0` of `flags`)가 set이면 search 결과가 비어도 SEARCH_RESPONSE를 보내야 함(payload `found=0`). Rust는 결과 있을 때만 reply. 클라이언트 retry timer가 MustReply에 의존하여 응답 누락을 NACK로 해석.

테스트: 2.

#### 2. `20d0d60` — DESTROY_CHANNEL on unknown SID silent drop

- **Anchor**: `CMD_DESTROY_CHANNEL` handler
- **Why**: pvxs `serverchan.cpp:382-386`: 알 수 없는 SID에 DESTROY_CHANNEL 도착 시 silent drop (echo reply 만들지 않음). Rust는 echo reply 조작 → 클라이언트가 본 적 없는 채널을 destroy 받음 → undefined behavior. CID-mismatch debug log 추가.

테스트: 2.

#### 3. `5a3245a` — PUT/GET/RPC data response가 client subcmd echo ★

- **Anchor**: `respond_with_data` / `handle_op` PUT/GET/RPC branches
- **Why**: pvxs `serverget.cpp:83`: data response의 subcmd 바이트는 클라이언트 request의 subcmd를 echo. PUT_GET처럼 EXEC + GET 비트가 동시에 set된 readback 모드에서 클라이언트는 send subcmd로 디스패치. Rust는 항상 INIT 후 EXEC만 → PUT_GET readback wire desync. 4 site 수정 (PUT, GET, RPC, PUT_GET 분기).

테스트: 2.

이 결함은 Rust 클라이언트가 PUT_GET을 안 써서 가려져 있었음 — pvxs C++ 클라이언트로 readback할 때만 발화.

#### 4. `80a447b` — GET_FIELD IOID collision with active op 거절 (P-G19 후속)

- **Anchor**: `CMD_GET_FIELD` handler
- **Why**: pvxs `serverintrospect.cpp:159` composite guard: GET_FIELD의 IOID가 active op과 충돌하면 거절. Rust는 silently overwrite → 두 op이 같은 IOID로 response 만들어 클라이언트 디스패처 confuse.

테스트: 2.

#### 5. `a6e63c6` — `PvaHeader::decode`가 `version=0` 거절

- **Anchor**: `PvaHeader::decode`
- **Why**: pvxs `pvaproto.h:687` `from_wire(Header)`는 `version=0`을 fault (메이저 버전 0은 reserved). Rust는 accept → 잘못 segmented frame을 parser가 디코드 시도.

테스트: 1.

**Deferred**:

- Frame direction-bit role-aware check (pvxs `conn.cpp:160`) — defense-in-depth, role plumbing refactor 필요
- CREATE_CHANNEL `count > 1` multi-name (pvxs `serverchan.cpp:269-358`) — Java 클라이언트가 spec 활용 가능
- `ackAny` pvRequest option (pvxs `servermon.cpp:553-580`) — feature parity, regression 아님
- decode-size `allow_null` gate — minor lenient acceptance

### Team D — `epics-ca-rs` ↔ `epics-base/modules/ca/` + `src/cas/`

#### 1. `9facef5` — repeater chained REGISTER + payload fan-out + `m_available` 게이트

- **Anchor**: `caRepeater` UDP receive loop
- **Why**: C `repeater.cpp:613-624`는 REGISTER 떼어내고 datagram 나머지를 등록 클라이언트로 fan-out — chained REGISTER+beacon-tunnel datagram 지원. Rust는 REGISTER 후 stop. 더불어 `m_available` rewrite를 RSRV_IS_UP 한정 (다른 명령에는 그대로 통과).

테스트: 5.

#### 2. `21240ad` — `send_ca_error` `m_cid` ↔ `m_available` 필드 swap ★★

- **Anchor**: `rg 'send_ca_error|CA_PROTO_ERROR'`
- **Why**: C `vsend_err` (`rsrv/camessage.c:139-224`): `m_cid = channel cid (or 0xFFFFFFFF for non-channel-scoped)`, `m_available = ECA status`. libca `exceptionRespAction` (`cac.cpp:1118`)이 status를 `m_available`에서 읽음. Rust는 status를 `m_cid`에 넣고 `m_available=0`을 그대로 둠 → C 클라이언트 측에서 모든 server-emit CA_PROTO_ERROR가 `ECA_NORMAL`(status 0)로 보임 → 에러가 silently 사라져 클라이언트는 응답 대기 stall. 함수 시그니처에 `chan_cid: u32` 파라미터 추가, 호출처에 looked-up cid 또는 0xFFFFFFFF sentinel 전달.

테스트: 1 (`proto_error_field_assignment_matches_c` wire golden).

#### 3. `90c56e8` — EVENT_CANCEL unknown sub-id → `ECA_BADMONID`

- **Anchor**: `CMD_EVENT_CANCEL` handler
- **Why**: C `event_cancel_action`은 unknown subscription-id에 CA_PROTO_ERROR + ECA_BADMONID 응답. Rust는 silent drop → 클라이언트가 잘못 cancel한 sub의 상태가 모호.

테스트: 1.

#### 4. `b7e7722` — TCP ECHO echoes full request header + payload

- **Anchor**: `CMD_ECHO` server handler
- **Why**: C `echo_action`은 받은 frame의 헤더+payload 전체를 그대로 echo. Rust는 bare zero header만 → keep-alive 검사 시 클라이언트가 응답 헤더/payload 동일성 확인하면 mismatch.

테스트: 1.

#### 5. `6ea50bd` — SEARCH reply `m_cid = ~0U` sentinel + TCP `m_postsize = 0`

- **Anchor**: `search_reply` builder
- **Why**: C `camessage.c::search_reply`: `m_cid = ~0U` sentinel (search-reply 모드). 기존 Rust는 `local_ip_for(src)` 임베드 — `0.0.0.0:0` 소켓을 클라이언트로 `connect`하여 커널이 고른 outgoing-interface IP를 사용. multi-homed 호스트에서 이 추측이 클라이언트가 실제로 reach한 인터페이스와 다를 수 있어 도달 불가 IP로 클라이언트를 보낼 위험이 있었음. C sentinel은 IP 결정을 수신 측에 위임(클라이언트가 보는 UDP source IP = 서버가 답한 인터페이스) → 커널이 정확. multi-NIC 라우팅 기능은 별도 경로로 보존: 서버 측 per-interface UDP binding(`server/addr_list.rs` + `server/udp.rs`의 `EPICS_CAS_INTF_ADDR_LIST` per-NIC responder task)이 SEARCH 도착 인터페이스에서 그대로 reply 송신하고, 클라이언트가 sentinel을 "UDP src IP 사용"으로 디코드하여 정확한 인터페이스 IP를 얻음. cap-tokens companion(`signed_beacon.rs`)은 직교한 인증 기능(Ed25519 over `server_ip‖port‖beacon_id‖ts`)으로, multi-NIC 라우팅과 무관. 더불어 TCP SEARCH-reply prefix에서 `m_postsize = 0`(minor-version trailer 없음) — C와 일치.

테스트: 2.

**Deferred**:

- Beacon counter sequence (C: `0, 0, 1, 2, ...`; Rust: `0, 1, 2, ...`) — Rust가 더 정확하고 prev F5 client `first_sighting force-rescan`이 duplicate 0 흡수.
- Channel-create 세분화 에러 코드 (`ECA_NORDACCESS`/`ECA_NOWTACCESS`/`ECA_BADTYPE`/`ECA_BADCOUNT`/`ECA_UNAVAILINSERV`) — semantic 차이, wire crash 아님
- Search burst behavior / `searchTimer.cpp` parity (UDP retry backoff)
- `CA_PROTO_ACCESS_RIGHTS` recomputation triggers — 별도 라운드

## 통합 시 발견된 결함

### `7ff7522` — 누락된 `send_ca_error` caller (Team D 머지 후)

- **Anchor**: `rg 'send_ca_error\(' crates/epics-ca-rs/src/server/`
- **원인**: Team D worktree가 `5f61621`(round-1 final)이 아닌 `1f1c45e`(main, round-1 이전)에서 분기 → round-1 commit `6b4d512`(HOST_NAME/CLIENT_NAME 512B caps + post-claim freeze)이 도입한 5개 caller가 Team D의 `rg send_ca_error` audit에 안 잡힘. Team D의 `21240ad`이 시그니처를 4-arg → 5-arg로 변경한 뒤, 머지가 auto-resolve clean이었지만(텍스트 충돌 없음) 통합 트리 컴파일 불가.
- **Same defect, fixed (5 site)**:
  - `tcp.rs:859` 미정렬 m_postsize reject
  - `tcp.rs:1091` HOST_NAME after-claim freeze
  - `tcp.rs:1111` HOST_NAME 512B cap
  - `tcp.rs:1163` CLIENT_NAME after-claim freeze
  - `tcp.rs:1180` CLIENT_NAME 512B cap
- 전부 non-channel-scoped → C `vsend_err` sentinel `0xFFFFFFFF` 전달.
- **Root cause (pre-existing)**: agent worktree branched from stale base. tool-level concern, 향후 라운드 시 `--from <commit>` 명시 또는 검증 절차 필요.

### `27e0bb0` — ReprocessAfter continuation이 PACT 가드 우회

- **Invariant**: 비동기 cycle owner의 continuation 재진입은 PACT 가드 우회. 외부 caller(FLNK, scan, CA put)는 가드 유지.
- **Owner/Gate**: `process_record_with_links_inner`에 `is_continuation: bool` 파라미터 추가. 공개 entry 둘:
  - `process_record_with_links` — foreign-caller, guard active (기존 의미 유지)
  - `process_record_continuation` — owner-driven, guard skip (신규)
- **원인**: Team A `16e0ff6`(PACT guard + AsyncPending sets pact)이 scaler-style 레코드에 회귀 — scaler `process()`는 DLY 대기 중 `AsyncPending + ReprocessAfter(remaining)` 반환. 타이머 만료 시 spawn된 task가 `process_record_with_links` 호출 → entry 가드가 pact=true 감지 → silent bail → `process()` 두 번째 호출 안 됨 → US가 WAITING에서 진행 못 함. 증상: `scaler-rs::test_scaler_dly_delayed_start` SS=0(NOT_COUNTING) 잔류, 기댓값 2(COUNTING).
- **Same defect, fixed**:
  - `processing.rs:1216` `ProcessAction::ReprocessAfter` spawn → `process_record_continuation` 호출
- **Distinct, skip**:
  - `complete_async_record` — pact를 미리 명시적으로 clear; bypass 불필요
  - `process_local` snapshot/db_test — 자체 swap-true, 별도 path
  - `dispatch_cp_targets` — 자체 PACT-busy check + RPRO=true, 자체 가드
- **C parity**: 동치는 `callbackRequestDelayed`가 `dbProcess`를 거치지 않고 `(*prset->process)(prec)`을 직접 호출하는 패턴. C는 가드 우회를 callback dispatcher 위치로 처리, Rust는 별도 entry API로 처리.

테스트: 1 추가 (`test_reprocess_after_continuation_bypasses_pact_guard`) — owner continuation이 pact=true에도 process() 재호출, 동시에 wait 중 foreign caller는 여전히 silent bail.

### `96ce0a7` — 워크스페이스 `cargo fmt --all` drift

각 팀이 자기 worktree에서 `cargo fmt --all`을 돌렸지만 crate-scoped 파일만 commit → 머지 후 5개 fmt-only delta 남음. 단일 style 커밋으로 갈무리, semantic 변경 없음.

### `db926af` — autosave .tmp 파일 실수 commit 제거

`27e0bb0` 작업 중 `git add -A`이 `examples/sim-detector/ioc/autosave/simDetector_settings.tmp`(runtime autosave 출력)를 같이 stage. 별도 커밋으로 제거.

## 머지 결과

```
db926af build: drop accidentally committed sim-detector autosave tmp
27e0bb0 fix(record): ReprocessAfter continuation bypasses PACT entry guard
7ff7522 fix(ca-server): update remaining send_ca_error callers for chan_cid arg
96ce0a7 style(workspace): cargo fmt --all post round-2 merges
c2aa1e6 Merge Team D: epics-ca-rs ↔ epics-base CA round 2
caee07a Merge Team C: epics-pva-rs ↔ pvxs C++ parity round 1
3f92b7e Merge Team B: asyn-rs ↔ asyn C parity round 2
0b9e8d5 fix(asyn-rs): InterruptValue carries aux_status + alarmStatus/Severity
a6e63c6 fix(pva): reject zero version byte in frame header (pvxs from_wire parity)
242a4ce feat(asyn-rs): TraceManager output_device_* + asyn_trace_device! macros
80a447b fix(pva): reject GET_FIELD when IOID collides with active op
5a3245a fix(pva): echo request subcmd in PUT/GET/RPC data response
6ea50bd fix(ca-server): SEARCH reply m_cid = ~0U sentinel, TCP postsize = 0
ad575cf fix(asyn-rs): strict get_*_strict variants + setter defined-flip on first write
54fc7ac feat(epics-base-rs): iocsh fd-numbered redirect parser (N> / N>>)
b7e7722 fix(ca-server): TCP ECHO echoes back request header + payload
531ec4f fix(epics-base-rs): substitute_macros backslash escape blocks \$ expansion
20d0d60 fix(pva): DESTROY_CHANNEL on unknown SID must not fabricate echo reply
90c56e8 fix(ca-server): EVENT_CANCEL with unknown sub-id replies ECA_BADMONID
3bea633 fix(pva): honour SEARCH MustReply flag + emit found=0 on empty reply
21240ad fix(ca-server): send_ca_error m_available carries ECA status, not m_cid
6dc7293 feat(epics-base-rs): calc record analog alarm limits + AFTC filter
9facef5 fix(ca-repeater): fanout remainder after stripped REGISTER + tighten m_available rewrite
16e0ff6 fix(epics-base-rs): dbProcess entry-level PACT guard + AsyncPending sets pact
```

## Round-3 후보

### `epics-base-rs`

- scan_event periodic_scan_loop 직렬 처리 (architecture change)
- iocsh `2>` true stderr capture (cross-cutting eprintln refactor)
- iocsh mid-line `<` redirect (SourceContext refactor)
- TPRO 진단 메시지 보강

### `asyn-rs`

- `create_param` strict `asynParamAlreadyExists` (+ ad-core-rs cascade)
- asynOctet `eom_reason` 전체 driver surface 변경
- asynUInt32Digital interrupt mask API (`uInt32RisingMask/FallingMask`, `setInterrupt/clearInterrupt/getInterrupt`)

### `epics-pva-rs`

- Frame direction-bit role-aware check
- CREATE_CHANNEL `count > 1` multi-name
- `ackAny` pvRequest option
- decode-size `allow_null` gate
- 양방향 모든 command pvxs 와이어 trace 대조 (PUT_GET 결함 같은 한 방향 hidden case 색출)

### `epics-ca-rs`

- Channel-create 세분화 에러 코드 (`ECA_NORDACCESS` class)
- Search burst backoff parity (`searchTimer.cpp`)
- `CA_PROTO_ACCESS_RIGHTS` recomputation triggers
- `--features cap-tokens` 빌드 에러 (`beacon_monitor.rs:303` `_src` typo) — round-1 heads-up, 미수정

## Tool-level / process 노트

- **Agent worktree branch base**: Team D worktree가 의도와 다른 base(`1f1c45e`)에서 분기. Agent tool의 `isolation: "worktree"` 호출 시 명시적 `--from <HEAD>` 또는 spawn 직후 base 검증 필요. 다음 라운드부터 강제.
- **Pipe + exit code**: `cargo ... 2>&1 | tail`은 tail의 exit code(0)를 반환 — 실제 결과 가려짐. 향후 `set -o pipefail` 또는 결과 파일에 redirect 후 별도 check.
- **kodex `learn` 저장**: send_ca_error field swap, SEARCH MustReply flag, subcmd-echo rule, ReprocessAfter continuation invariant 등 핵심 패턴 모두 그래프에 적재(에이전트별 `learn` 호출).
