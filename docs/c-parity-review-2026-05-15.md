# C parity review — 2026-05-15 multi-team round

3개 서브에이전트 팀이 worktree 격리에서 병렬로 `crates/epics-base-rs`, `crates/epics-ca-rs`, `crates/asyn-rs`를 C 원본과 대조해 리뷰·수정·커밋한 결과. 모두 main에 머지되었다.

참조 소스:

- `~/codes/epics-base/` — libCom, dbStatic, database, modules/ca, src/cas
- `~/codes/epics-modules/asyn/` — asyn driver framework

방법론은 글로벌 룰 *Fixes from reported defects* — 각 finding마다:

1. 결함의 구조적 anchor 식별
2. workspace-wide `rg`
3. 모든 hit을 `same defect (fix now)` / `distinct (one-line why)` 분류
4. same defect 모두 한 커밋으로 묶음 fix
5. nextest + doctest 그린 후 커밋

각 finding 섹션의 **Audit trail** 블록이 그 분류 결과다.

## 라운드 결과 요약

| 팀 | 크레이트 | 커밋 | 머지 | 테스트 |
|---|---|---|---|---|
| A | `epics-base-rs` | 4 | `8438594` | 1140→1157 nextest, +17 회귀 |
| B | `epics-ca-rs` | 4 | `afa91b3` | 177/177 nextest |
| C | `asyn-rs` | 4 | `9d1a6ce` | 393/393 nextest |

통합: `cargo build --workspace` clean, `cargo nextest -p epics-base-rs -p epics-ca-rs -p asyn-rs` **1727/1727 PASS** (15 skipped), doctest 0 fail.

## 수정된 finding

### Team A — `epics-base-rs` ↔ `epics-base/modules/{database,libcom}/src/`

#### 1. `544ae66` — ai/ao SPC_LINCONV parity (LINR/EGUF/EGUL)

- **Anchor**: `"LINR" | "EGUF" | "EGUL"` put_field 분기
- **Why**: C `aiRecord.c:181-200` / `aoRecord.c:242-267`은 LINR/EGUF/EGUL을 `special(SPC_LINCONV)`로 태그. put 시 `init=TRUE`로 SMOO 필터를 reprime하고 LINEAR 모드일 때 `eoff = egul`로 rebase. Rust 포트는 stale `eoff`를 그대로 두어 다음 RVAL↔VAL 변환부터 오프셋이 어긋났음. ai는 `init` clear도 추가 (smoothing pre/post 혼합 방지). ao는 smoothing 없으니 init 무관.

**Audit trail**

- **Same defect, fixed**:
  - `ai.rs:364-386` (LINR/EGUF/EGUL 세 분기)
  - `ao.rs:538-558` (LINR/EGUF/EGUL 세 분기)
- **Distinct, skip**:
  - `longin/longout/int64in/int64out` — LINR 없음 (정수 raw=eng)
  - `mbbi/mbbo` 등 — enum, 선형화 없음
  - `calc/calcout` 등 — raw 변환 단계 자체가 없음

테스트: ai/ao에 5개 추가 (init-clear, LINEAR하 eoff rebase, SLOPE 보존).

#### 2. `63b4551` — iocsh `${NAME}` brace form (macLib parity)

- **Anchor**: `substitute_env_vars` + `find_closing_paren` (`$` 다음 `(`)
- **Why**: C `macLib` (`macCore.c:777`)은 `$(NAME)`과 `${NAME}` 둘 다 인식 — `(*r++ == '(') ? "=,)" : "=,}"`. Rust `db_loader::substitute_macros`는 이미 둘 다 지원, 그러나 `iocsh::substitute_env_vars`는 `$()`만 — `dbLoadRecords ${IOC}` 같은 site-local st.cmd 컨벤션이 리터럴 `${IOC}`로 dbLoadRecords에 전달됐음. `find_closing_paren`도 `${...}` 인식하도록 확장 (macro body 안의 `)`가 외곽 paren 종료로 잘못 잡히지 않도록).

**Audit trail**

- **Same defect, fixed**:
  - `iocsh/registry.rs:397` `substitute_env_vars`
  - `iocsh/registry.rs:239` `find_closing_paren`
- **Distinct, skip**:
  - `db_loader/mod.rs:240` `substitute_macros` — 이미 두 form 모두 처리
  - `autosave/macros.rs:56` — 이미 `${}`와 `$$` 처리

테스트: `substitute_env_vars_handles_brace_form` (`serial(epics_env)`) — env 확장, 기본값 `${VAR=default}`, miss 시 verbatim passthrough, 레거시 `$()` 호환.

#### 3. `73880c7` — recGblResetAlarms ACKS auto-raise + INVALID clamp

- **Anchor**: `rec_gbl_reset_alarms` (정의 한 곳, 호출자 walk)
- **Why**: C `recGblResetAlarms` (`recGbl.c:178-224`)는 두 가지를 수행하는데 Rust 포트가 누락. (1) `nsev > INVALID_ALARM`을 INVALID로 clamp (`:188-189`) — NSEV에 stray put이 들어와도 sevr이 알 수 없는 값으로 오염되지 않음. (2) ACKT/ACKS sticky 규칙: `!ackt || new_sevr >= acks`일 때 ACKS를 새 severity로 raise (`:209-217`). 디폴트 ACKT=YES (sticky)는 ACKS가 raise만 되고 operator의 CA put으로만 clear — 이거 없으면 sticky alarm-handler 워크플로 사실상 사망.

**Audit trail**

- **Same defect, fixed (모든 호출자)**:
  - `recgbl.rs:128` 정의
  - `database/processing.rs:800,838` post-completion alarm reset (async 양 분기)
  - `record_instance.rs:1525` `process_local` alarm reset
- **Distinct, skip**:
  - `field_io.rs::alarm_changed` — put-time 체크지 reset 경로 아님. C도 ACKS 갱신은 recGblResetAlarms 안에서만 함.
  - `record_instance.rs:755` ACKS put_field 핸들러 — operator acknowledge 경로 (ACKS 쓰기 = clear). 무관.

테스트: 3 (first-alarm raise / ACKT=YES sticky keep / ACKT=NO transient track).

#### 4. `d9aa8ea` — check_deadband NaN/infinity 전이 발화

- **Anchor**: `(val - mlst).abs() > mdel` / `(val - alst).abs() > adel` + `check_deadband_ext`
- **Why**: C `recGblCheckDeadband` (`recGbl.c:345-370`)는 4분기 — (양쪽 finite) / (한쪽만 NaN/inf) / (반대 부호 inf) / (둘 다 NaN, 같은 부호 inf). Rust는 `mdel < 0.0 || mlst.is_nan() || (val - mlst).abs() > mdel`로 평탄화 → `newval=NaN, oldval=finite` 케이스에서 `(NaN - finite).abs() = NaN`, `NaN > deadband = false` → calc divide-by-zero / link timeout 등으로 UDF 갈 때 monitor 무발화. camonitor/pvmonitor가 마지막 유효값 보고 fault 못 봄. 4분기를 명시한 자유함수 `check_deadband` 추출, MDEL/ADEL 모두 위임.

**Audit trail**

- **Same defect, fixed**:
  - `record_instance.rs:1633-1634` (VAL/monitor + VAL/archive 두 트리거 — 헬퍼로 통합 `:1882`)
- **Distinct, skip**:
  - `aftc_filter` 시상수 rounding — 별개 시맨틱 (alarm range filter, monitor deadband 아님)
  - `last_posted` 동등 비교 — byte-exact, 수치 deadband 아님

테스트: 8 (NaN-old sentinel, finite within/beyond, 음수 deadband short-circuit, NaN-new+finite-old (이번 수정의 핵심), 한쪽 inf, 반대 부호 inf, 같은 부호 inf).

### Team B — `epics-ca-rs` ↔ `epics-base/modules/ca/` + `src/cas/`

#### 1. `7fcc26e` — extended-header threshold off-by-one

- **Anchor**: `count > 0xFFFF` + `from_bytes_extended` 안의 `count == 0`
- **Why**: C `comQueSend.cpp:285` — `payloadSize < 0xffff && nElem < 0xffff`이 FALSE면 extended. 즉 둘 중 하나라도 정확히 `0xFFFF`면 extended branch. Rust gate `count > 0xFFFF`는 under-trigger — `count == 0xFFFF`에서 normal-form `m_count = 0xFFFF`를 쓰면 strict peer는 8B annex 빠진 extended marker로 읽음. `size > 0xFFFE`는 `>= 0xFFFF`와 같으니 size 쪽은 우연히 맞았고 count만 drift. 둘 다 명시. 더불어 `from_bytes_extended`/`is_extended`/`actual_postsize`/`actual_count`가 `m_count == 0`까지 요구 — C `tcpiiu.cpp:1168`/`cac.cpp:1097`/`rsrv/camessage.c:2410`는 postsize만 본다. count==0 강제는 합법 extended (스펙은 emitter가 0 쓰지만 receiver는 lenient여야 함) 거절.

**Audit trail**

- **Same defect, fixed (전 5)**:
  - `protocol.rs::set_payload_size`, `is_extended`, `actual_postsize`, `actual_count`, `from_bytes_extended`
  - `client/mod.rs::build_read_notify_frame`
  - `client/transport.rs` READ_NOTIFY + EVENT_ADD
- **Distinct, skip**: 없음 — 모든 gate가 동일한 off-by-one.

테스트: `protocol_tests::header_set_payload_count_boundary_at_0xffff`가 이전엔 `count==0xFFFF` normal로 단언 — 그게 결함이었지 spec 아니었다. extended로 뒤집음. `test_extended_count_overflow`에 0xFFFF 회귀 추가.

#### 2. `7c49850` — VERSION reply zero-fill + beacon `m_available=0`

- **Anchor**: `CA_PROTO_VERSION =>` server emit + `RSRV_IS_UP` beacon
- **Why**: 두 byte-exact deviation. (1) `rsrv_version_reply` (`camessage.c:2115`)는 `m_count = CA_MINOR_PROTOCOL_REVISION` 외 전부 0. Rust는 `data_type=1, cid=1, count=13` — 실제로 C 클라이언트(`tcpiiu.cpp::versionRespNotify`)는 m_count만 봐서 무해했지만 wire-trace 차이. (2) `RSRV_IS_UP` beacon `m_available` (`online_notify.c:69-72`): C는 memset 0 후 cmmd/count(port)/dataType(minor ver)/cid(seq)만 set, `m_available`은 INADDR_ANY. C 클라이언트 `udpiiu.cpp:762`가 명시: "new servers: always set this field to INADDR_ANY"; 비-zero는 OVERRIDING 서버 IP로 해석되어 source-address 자동 해석(NAT, multi-NIC, repeater fan-out)을 우회. Rust는 probe-derived `server_ip` 박아 (a) byte drift, (b) 스펙상 OLD server 시그널, (c) multi-homed에서 probe 목적지가 모든 수신자 NIC을 안 거치면 잘못된 IP. signed-beacon companion(cap-tokens)은 시그니처가 IP를 묶으니 probe-derived 유지.

**Audit trail**

- **Same defect, fixed (전 3)**:
  - `server/tcp.rs::CA_PROTO_VERSION` 핸들러
  - `server/beacon.rs::run_beacon_emitter`
  - `tests/wire_golden.rs::rsrv_is_up_beacon` 골든
- **Distinct, skip**:
  - `server/udp.rs::CA_PROTO_VERSION` (UDP search-reply VERSION prefix) — 이미 정확 (count만 set)

#### 3. `6b4d512` — HOST_NAME/CLIENT_NAME 512B cap + post-claim freeze

- **Anchor**: `CA_PROTO_HOST_NAME =>`, `CA_PROTO_CLIENT_NAME =>` 분기
- **Why**: C `camessage.c::host_name_action`/`::client_name_action` 두 케이스에서 거절. (1) 첫 채널 claim 후 identity 갱신은 ECA_INTERNAL + 메시지("attempts to use protocol to set host/user name after creating first channel ignored by server") — 클라이언트가 한 identity로 채널 만들고 hostname/username 재기록해 ACF 권한을 `reeval_access_rights`로 escalation하는 path 봉쇄. (2) Payload 길이 511B cap (C는 `size > 512`, `size = strnlen(pName, m_postsize) + 1`) — 초과시 ECA_INTERNAL("bad (very long) host name" / "very long user name"). Rust는 transport `MAX_PAYLOAD_SIZE`(16 MiB)까지 받아 verbatim 저장 → connection-spam 시 메모리 grow DoS.

**Audit trail**

- **Same defect, fixed (양쪽)**:
  - `server/tcp.rs::CA_PROTO_HOST_NAME` 핸들러
  - `server/tcp.rs::CA_PROTO_CLIENT_NAME` 핸들러
- **Distinct, skip**:
  - CREATE_CHAN PV-name 길이 — 이미 record/PV lookup으로 cap (수백 바이트 넘으면 PV-not-found).

#### 4. `f690729` — 미정렬 m_postsize 거절 (`& 0x7 != 0`)

- **Anchor**: `align8(.*postsize)` / `align8(actual_post)` recv 경로
- **Why**: C `tcpiiu.cpp:1198` + `rsrv/camessage.c:2452`는 `m_postsize & 0x7 != 0` reject. Wire spec은 모든 payload 8B align. 미정렬은 malformed거나 파서를 다음 프레임 중간으로 slip시키려는 시도. C는 TCP면 conn close, UDP면 datagram drop, "CAS: Missaligned protocol rejected" / "misaligned" 진단 emit. Rust는 `align8(actual_post)`로 round-up — peer가 `m_postsize=5` 보내면 8B consume → 다음 메시지 헤더로 walk. 적대적 peer에 대해 wire-byte-level desync.

**Audit trail**

- **Same defect, fixed (전 6)**:
  - `server/tcp.rs` framing 루프 — TCP 서버, CA_PROTO_ERROR + ECA_INTERNAL + drop conn (`camessage.c:2452` TCP branch)
  - `client/transport.rs` framing 루프 — TCP 클라이언트, drop conn (`tcpiiu.cpp:1198`)
  - `client/search.rs` TCP-name-server reader — drop datagram, reconnect
  - `client/search.rs` UDP SEARCH-reply dispatcher — silently break (C UDP branch)
  - `server/udp.rs` UDP SEARCH receiver — silently break (C UDP branch)
  - `client/beacon_monitor.rs` UDP beacon receiver — silently break (UDP)
- **Distinct, skip**: 없음 — `align8(postsize)`를 receive 측에서 쓰던 모든 loop가 동일 lax acceptance.

### Team C — `asyn-rs` ↔ `epics-modules/asyn/asyn/asynDriver/`

#### 1. `e3d481d` — trace setter → `asynExceptionTrace*` 발화

- **Anchor**: `set_trace_mask|set_trace_io_mask|set_trace_info_mask|set_trace_file|set_io_truncate_size|set_device_trace_mask`
- **Why**: C asyn은 모든 setTraceXxx에서 `announceExceptionOccurred(... asynExceptionTrace{Mask,IOMask,InfoMask,File,IOTruncateSize})` 호출 (`asynManager.c:2790/2800, 2832/2842, 2874/2884, 2923, 2956`). Rust는 config in-place mutate만 → asynShellCommands UI / asynRecord `TMSK TIOM TINF TFIL TSIZ` refresh / monitor relay가 변경 못 봄. `Option<Arc<ExceptionManager>>` sink를 TraceManager에 주입, PortManager 생성자가 자기 ExceptionManager 같은 걸 install. per-device는 addr 운반, 글로벌 path는 `port_name=""`로 fire. set_trace_file과 set_io_truncate_size는 C 컨벤션 (`asynManager.c:2923/2956`이 `puserPvt->pport`로 게이트 — 글로벌 path에선 fire 안 함) 보존.

**Audit trail**

- **Same defect, fixed (6 setter 전부)**:
  - `trace.rs:319` `set_trace_mask`
  - `trace.rs:399` `set_trace_io_mask`
  - `trace.rs:417` `set_trace_info_mask`
  - `trace.rs:435` `set_trace_file`
  - `trace.rs:453` `set_io_truncate_size`
  - `trace.rs:471` `set_device_trace_mask`
- **Distinct, skip**:
  - `asyn_record/mod.rs:840-883` 호출자 — consumer로서 이번 수정으로 자동으로 fan-out 받게 됨.

테스트: 3 (`test_set_trace_mask_fires_exception`, `test_global_trace_mask_announce`, `test_global_file_and_truncate_do_not_announce`).

#### 2. `e7c36fc` — connect/disconnect 상태 전이일 때만 announce + `set_auto_connect`

- **Anchor**: `fn connect|fn disconnect|fn connect_addr|fn disconnect_addr|auto_connect`
- **Why**: C `exceptionConnect` (`asynManager.c:2151-2160`) / `exceptionDisconnect` (`:2174-2185`)는 no-op call에 asynError 리턴 + 실제 상태 변화일 때만 `asynExceptionConnect` announce. Rust는 무조건 fire → re-subscribe / re-arm idle / CA gateway reconnect 등 edge-triggered listener가 driver의 state 재확인마다 spurious dup 봄. State-change guard 추가, idempotency 위해 Ok 리턴 유지. `set_auto_connect_*`는 신규 — C `autoConnectAsyn` (`asynManager.c:2310-2324`)는 항상 `asynExceptionAutoConnect` fire (no-edge guard); 드라이버 생성자가 register 전 `base.auto_connect` 직접 대입하는 건 listener 없으니 silent path 유지.

**Audit trail**

- **Same defect, fixed (port.rs 4 site)**:
  - `port.rs:569` `connect` (trait default)
  - `port.rs:575` `disconnect` (trait default)
  - `port.rs:293` `PortDriverBase::connect_addr`
  - `port.rs:299` `PortDriverBase::disconnect_addr`
- **Distinct, skip (드라이버 생성자 — listener 부착 전이라 silent path 정확)**:
  - `ftdi.rs:152`, `vxi11.rs:236`, `serial_port.rs:430`, `prologix.rs:134`, `ip_port.rs:396`, `ip_server_port.rs:334+915`, `usbtmc.rs:210`, `port_actor.rs:706`

테스트: `test_connect_disconnect_announce_only_on_transition`, `test_set_auto_connect_fires_unconditionally`.

#### 3. `6a2b7ca` — OctetWriteRead flush 우선

- **Anchor**: `OctetWriteRead`
- **Why**: C `asynOctetSyncIO::writeRead` (`asynOctetSyncIO.c:250`)는 한 `queueLockPort` 안에서 flush() → write() → read(). flush가 input buffer의 stale 바이트(serial echo, half-received response, prompt banner)를 비워 post-write read가 *이번* command 응답만 받게 함. Rust는 flush 생략 → line이 warm이면 pre-existing input이 응답에 새서 command-response 프로토콜 corrupt. PortActor의 dispatch_io가 actor 전용 스레드에서 도니 flush+write+read 자연스러운 atomic (queueLockPort 등가물 불필요).

**Audit trail**

- **Same defect, fixed**:
  - `port_actor.rs:285` 핸들러
- **Distinct, skip**:
  - `request.rs:50` variant decl — 변경 없음
  - `protocol/{convert,command}.rs` — wire serialization, flush는 dispatch-time driver 관심사

테스트: `actor_octet_write_read_calls_flush_first` — `FlushTracker` 드라이버로 호출 시퀀스가 정확히 flush → write → read임을 단언 (단순 flush 호출 여부가 아니라).

#### 4. `3766c2c` — `set_connected` owner-API로 Connect-edge invariant closure

- **Invariant**: `base.connected` (port-level) / `device_state.connected` (per-addr) 전이는 edge-guarded이어야 하고 `AsynException::Connect`는 transition당 정확히 한 번만 fire.
- **Owner/Gate**: `PortDriverBase::set_connected(yes)` (port slot), `PortDriverBase::set_addr_connected(addr, yes)` (per-addr slot). 둘 다 prior state 비교, 변화시에만 mutate + announce. fan-out 발생 여부를 `bool`로 리턴.
- **Bypass audit anchor**: `rg "base\.connected\s*=|base_mut\(\)\.connected\s*=|announce_exception(AsynException::Connect"`

**Audit trail**

- **Same defect — owner로 라우팅 (9 site, 5 file)**:
  - `ip_port.rs:597, 617-619, 643-645, 677-679, 739-742`
  - `serial_port.rs:549-550, 581-582`
  - `ip_server_port.rs:373-374, 442-443, 665-666, 949-952, 973-974`
  - `prologix.rs:317, 325`
- **Distinct, skip — init/생성자 (8 site, listener 부착 전이라 edge irrelevant)**:
  - `ftdi/vxi11/usbtmc/ip_*/serial/prologix::new()`
  - `ip_server_port:911` subport new
- **Distinct, skip — slot-management announce (6 site, slot이 자체 edge 소유)**:
  - `ip_server_port:529, 548, 651, 708, 720, 751` `announce_exception(Connect, addr)` — `slot.is_occupied()` 체크가 자체 edge guard. slot은 PortDriverBase device_state와 독립.

**Structural closure**: `set_connected`/`set_addr_connected` 신규 + `connect`/`disconnect` trait default가 둘을 통해 라우팅. `connect_addr`/`disconnect_addr`는 thin wrapper. 393 테스트 그린, 회귀 없음.

## Deferred — 다음 라운드 후보

### Team A — `epics-base-rs`

1. **calc record AFTC filter 미연결** — `record_instance::aftc_filter` 헬퍼는 있으나 calc record_type이 호출하지 않음.
2. **process_record_with_links_inner entry-level PACT guard 누락** — `record.process()`가 swap_true 게이트 없이 호출됨. 비동기 완료 후 chain이 RPRO=true로 deferred되지 않고 dual-fire.
3. **substitute_macros 백슬래시 escape 부재** — C `macLib trans`는 `\` 처리.
4. **iocsh redirection 부분 지원** — Rust는 `>`, `>>`만. C는 `<` mid-line + `N>`, `N>>`도.
5. **scan_event periodic_scan_loop 직렬 처리** — 동일 scan_type 안에서 PACT-pending record 하나가 group 전체를 블록.
6. **calc record 알람 한계 필드 부재** — hihi/high/low/lolo 미구현.

### Team B — `epics-ca-rs`

1. **Repeater chained REGISTER + payload datagram** — C `repeater.cpp:613-624`는 REGISTER 떼어내고 나머지를 fan-out. Rust는 REGISTER 후 stop. Edge case (chained beacon-tunnel datagram), 실제 노출 빈도 낮음.
2. **Server SEARCH reply `m_cid` 차이** — C 기본 rsrv는 `sid = ~0U`. Rust는 `local_ip_for(src)` 임베드. 두 모드 모두 libca 문서화. multi-NIC 라우팅 이점 있는 의도적 차이지만 byte-exact 아님 — upstream guidance 변경 시 재검토.

### Team C — `asyn-rs`

1. **asynOctet eom_reason 미전파** — interpose stack은 내부적으로 `EomReason` 운반하지만 trait이 `usize`로 truncate. Surface 변경이 모든 driver에 파급 — 한 라운드에 너무 넓다.
2. **asynUInt32Digital 인터럽트 stub 부재** — `setInterrupt/clearInterrupt/getInterrupt/registerInterruptUser` 없음. Feature gap (회귀 아님).
3. **`ParamList::get_int32` undefined → 0 반환** — C asyn은 `paramValNotDefined` throw / `asynParamUndefined` 반환. 호출처와 테스트가 lax behavior에 의존, 변경은 cascade.
4. **`ParamList::create_param` 중복 이름 silent re-use** — C는 `asynParamAlreadyExists`. AsynParamSet/PVI 흐름이 idempotent read에 의존.
5. **TraceManager `output / output_with_source / output_io`가 device-level config 미참조** — port → global만 참조. `is_enabled_device`는 device override를 본다. 현재 device trace 호출처 없음.
6. **asynUser `error_message` / `aux_status` 필드 부재** — Rust는 `AsynError::Status { message }`로 대체 — 충분.

## Upstream / port-back 노트

- **Pre-existing**: `--features cap-tokens` 빌드 에러 — `crates/epics-ca-rs/src/client/beacon_monitor.rs:303`의 `_src` typo. 이번 라운드와 무관, 별도 티켓 권장.
- **Wire 골든 업데이트**: `tests/wire_golden.rs::rsrv_is_up_beacon`이 C-spec(`available=0`) 반영. 이제 진짜 reference vector 역할.
- **테스트 가정 정정**: `protocol_tests::header_set_payload_count_boundary_at_0xffff`가 이전엔 `count==0xFFFF`를 normal form으로 단언했음 — 그게 결함이었지 spec이 아니었다. extended-form 단언으로 뒤집음.
- **kodex `learn` 저장**: extended-header threshold, beacon `m_available`, misaligned-postsize rule (Team B), trace/connect/octet/invariant patterns (Team C).
- **C 원본 경로 정정** (Team A): 실제 C 소스는 `~/codes/epics-base/modules/{database,libcom}/src/`. `~/codes/epics-base/src/`는 install tooling만 보유.

## 머지 결과

```
9d1a6ce Merge Team C: asyn-rs ↔ asyn C parity (trace/connect/octet)
afa91b3 Merge Team B: epics-ca-rs ↔ epics-base CA wire/server parity
8438594 Merge Team A: epics-base-rs ↔ epics-base C parity round 1
3766c2c refactor(asyn-rs): set_connected owner-API closes Connect-edge invariant
d9aa8ea fix(record): check_deadband — fire on NaN/infinity transition (recGbl.c parity)
f690729 fix(ca-wire): reject misaligned m_postsize on receive — C `& 0x7` rule
73880c7 fix(recgbl): rec_gbl_reset_alarms — INVALID_ALARM clamp + ACKS auto-raise
6b4d512 fix(ca-server): enforce HOST_NAME/CLIENT_NAME caps + post-claim freeze (C parity)
6a2b7ca fix(asyn-rs): OctetWriteRead flushes input first — match C asyn
63b4551 fix(iocsh): substitute_env_vars accepts ${NAME} brace form — macLib parity
7c49850 fix(ca-server): VERSION reply zero-fill + beacon m_available=0 (C parity)
544ae66 fix(ai,ao): SPC_LINCONV parity — LINR/EGUF/EGUL puts rebase eoff + reprime SMOO
7fcc26e fix(ca-wire): extended-header threshold — `>= 0xFFFF`, not `> 0xFFFF`
e7c36fc fix(asyn-rs): connect/disconnect announce only on edge — match C asyn
e3d481d fix(asyn-rs): trace setters fire asynExceptionTrace* — match C asyn
```
