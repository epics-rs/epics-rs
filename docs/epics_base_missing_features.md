# EPICS Base & Asyn 변경 사항 중 `epics-rs` 미반영 항목 총정리 (과거 내역 포함)

`epics-rs` 프로젝트의 `docs/upstream-tracking.md` 및 `epics-base`, `asyn` (2015년~현재)의 PR, Issue, Commit 전체 내역을 심층 분석한 결과, 아직 러스트 환경(`epics-rs`)에 반영되지 않았거나 추적/개발 중인 미반영 항목들은 다음과 같습니다.

> **Status legend** (항목 머리에 부착되는 마커)
> - ✅ **DONE** `<commit>` — 본 작업에서 새로 구현. 괄호의 commit hash는 `feat/upstream-features` 브랜치의 SHA.
> - ⏭️ **ALREADY** — 기존 코드베이스에 이미 구현되어 있던 항목. 점검만 수행.
> - 🔄 **PARTIAL** `<commit>` — 부분 구현. 한계는 본문에 명시.
> - ⏸️ **DEFERRED** — 별도 PR 필요. scope/이유는 본문에 명시.
> - ⚠️ **N/A** — 적용 불필요 (디자인 차이, wire-format 제약 등).

## 진행 요약 (2026-05-13 세션)

`feat/upstream-features` 브랜치에 11 commits, 3245 tests pass, fmt+clippy 클린.

| Commit | 항목 |
|---|---|
| `9d8a34b` | TCP/UDP 서버 포트 분리 (`EPICS_CAS_SERVER_PORT`, PR #69) |
| `ae277d1` | `EPICS_CA_MCAST_TTL` UDP 멀티캐스트 TTL (3.16 f2a1834d) |
| `8615bb4` | `EPICS_IOC_IGNORE_SERVERS` 클라이언트 quarantine (6efe2924) |
| `6862ef0` | ACF HAG DNS soft fallback (libcom 932e9f3) |
| `17210b4` | dbPut/dbGet 16진수·8진수 (Double/Float, PR #678) |
| `7ed3baf` | `getenv` 내장 디바이스 (3.15.4) |
| `23360e6` | mTLS → ACF METHOD/AUTHORITY (PR #641) |
| `a409311` | 절전 wake echo probe 단축 (Issue #190) |
| `73b517c` | longout OOPT 조건부 출력 (7.0.8) |
| `a02c310` | aai/aao/subArray 레코드 (PR #162/#742) |
| `ac92e3e` | SIMM=RAW + dbServerStats 카운터 확장 |

미해결 핵심 deferred 항목:
- IPv6 완전 지원 (PR #205) — ~2000 LOC, 별도 multi-day PR
- 서버 측 채널 필터 (3.15.7) — ~1000 LOC + 설계 리뷰
- `aSub` constant INP / `bi`-`bo` mask 변환 / 다수의 9-A high-priority 라이프사이클 fix

---

## 1. 네트워킹 및 CA / PVA 프로토콜 (Networking)
- **IPv6 지원 (PR #205)** — 🔄 **PARTIAL (Stages 1-6)**: PVA TCP/UDP 서버 IPv6 동작 + 클라이언트 v6 SEARCH 송수신 + v6 multicast group (FF0E::400) + 서버 v6 beacon emit + **클라이언트 v6 beacon recv**. **Stage 1**: `PvaServerConfig::bind_ip: Ipv4Addr` → `IpAddr` 일반화 (`crates/epics-pva-rs/src/server_native/runtime.rs:35`), `[::1]:5075`/`[::]:5075` TCP bind 가능; `PvaServer::client_config()`는 server bind family를 따라 v4/v6 loopback 자동 선택; pva-gateway-rs/dual-gateway-rs binaries `--bind ::1` 수용. **Stage 2**: 신규 `run_udp_responder_v6` (`udp.rs`) — `enable_ipv6_udp = true`일 때 `PvaServer::start`가 `[::]:udp_port` 별도 listener task 추가 spawn, v6 SEARCH 수신 후 동일 GUID/TCP-port로 SEARCH_RESPONSE. v4 path와 parallel 운영 (AsyncUdpV4 미수정 — non-invasive). **Stage 3**: 클라이언트 search engine이 `EPICS_PVA_ADDR_LIST`의 v6 항목을 search-time에 일괄 처리, Stage 4 도입 전에는 v6 socket 부재 시에만 drop+warn. **Stage 4**: 클라이언트 ephemeral v6 socket (`bind_ephemeral_udp_v6`, `[::]:0` + `IPV6_V6ONLY=1`) 추가, `broadcast()`가 `SocketAddr::V6` 대상을 v6 socket으로 라우팅, 별도 recv arm 추가로 v6 SEARCH_RESPONSE 수신. `AUTO_ADDR_LIST=YES`이면 `[ff0e::400]:5076` 멀티캐스트 그룹 자동 추가. `rewrite_loopback`은 v6 peer면 v6 loopback을 유지 (이전엔 v4 LOCALHOST로 하드코딩됨). 서버 측 `process_v6_search_datagram`는 wire의 `reply_port` 대신 UDP source port에 응답 (v6 unicast의 자연스러운 동작). PVA wire는 이미 16-byte IPv4-mapped IPv6 인코딩(`proto/ip.rs::ip_to_bytes`). Linux 기본 `IPV6_V6ONLY=0` 자동 dual-stack, BSD/macOS는 v6-only. **Stage 5**: 서버 beacon emitter (`run_udp_responder_with_config`)가 `enable_ipv6_udp=true && auto_beacon=true`일 때 `[ff0e::400]:udp_port`를 beacon_destinations에 자동 추가하고, 별도 v6 send 소켓 (`bind_beacon_send_v6`, `[::]:0 + IPV6_V6ONLY=1`)으로 v6 destination을 라우팅. seq/change_count는 동일 beacon wave 안에서 v4/v6 destination 양쪽에 공유 — 동일 server identity. 5 integration tests (v6 TCP bind+dial, v6 end-to-end pvget round-trip, v6 SEARCH→SEARCH_RESPONSE wire-level, v6 search engine → 서버 라운드트립 resolve, **신규** beacon emit reaches explicit v6 destination). **Stage 6**: 클라이언트 v6 beacon recv — `bind_beacon_udp_v6`가 `[::]:broadcast_port`에 `SO_REUSEADDR/SO_REUSEPORT + IPV6_V6ONLY=1`로 바인드, `[ff0e::400]` multicast 그룹 자동 가입; `run_engine`의 select!에 v6 beacon recv arm 추가 (동일 `handle_beacon` 디코더 재사용 — beacon 페이로드는 family-agnostic). 레거시 `client_native/search.rs::rewrite_loopback_target`도 family-aware로 동시에 정리. 잔여 `Ipv4Addr` 식별자 사용처(~100건)는 대부분 (a) 테스트 fixture, (b) IPv4-specific 상수 (`BROADCAST`, NIC enumeration), (c) `0.0.0.0:0` 같은 v4-typed 기본값으로, IPv6 동작을 막지 않음 — 무차별 변환은 churn일 뿐 의미 있는 동작 변화가 없어 보류. 통합 테스트 추가: `v6_beacon_socket_binds_and_joins_default_group` (binder), `v6_beacon_arriving_at_engine_is_observed_by_tracker` (end-to-end recv → BeaconTracker). **CA는 IPv4 전용으로 확정** — CA wire header의 `available`/`cid` 필드가 u32 (4-byte) 이므로 RSRV_IS_UP beacon body / SEARCH_REPLY가 v6 주소를 표현할 수 없음 (protocol-locked). Wire audit: `crates/epics-ca-rs/src/protocol.rs:299` (`hdr.available` u32), `server/beacon.rs:92`, `server/udp.rs:163`. Upstream PR #205도 동일 이유로 EPICS_HAS_IPV6=1 활성 영역을 PVA로 한정.
- **DNS 변경 시 영구 연결 끊김 현상 (Issue #488)** — ⏭️ **ALREADY**: round-50 작업으로 `EPICS_CA_DNS_REFRESH_SECS` + `AddrEntry::refresh_dns` 구현됨 (`crates/epics-ca-rs/src/client/search.rs:354`).
- **TLS 기반 보안 pvAccess (PR #641)** — ✅ **DONE** `23360e6`: `tls::issuer_from_cert` 추가, `ClientState{auth_method, auth_authority}` 필드, `compute_access`를 `check_access_method`로 전환. mTLS peer cert의 issuer DN이 `AUTHORITY()` ACF 절과 매칭되며 method는 `"x509"` 고정.
- **CA 클라이언트의 서버 프로토콜 버전 결정 (PR #711)** — ⏭️ **ALREADY**: `transport.rs`가 CA_PROTO_VERSION 수신 시 `server_minor_version`을 캡처하고 `send_echo`에서 v4.3+ ECHO vs 이전 READ_SYNC로 분기.
- **지정된 TCP 포트 + UDP 5064 분리 (PR #69)** — ✅ **DONE** `9d8a34b`: `cas_server_port()`, `CaServerBuilder::tcp_port`, `IocApplication::tcp_port`. UDP 응답기가 실제 바인딩된 TCP 포트를 SEARCH_REPLY에 광고.
- **절전 모드(Suspend) 해제 후 CA 멈춤 현상 (Issue #190)** — ✅ **DONE** `a409311`: wall-clock skip 기반 suspend wake 탐지, echo probe 5s→1s 단축, tracing::info 기록. 절전 후 복구 ~1s.
- **서버 측 채널 필터 (Server-side Filters, 3.15.7)** — 🔄 **PARTIAL**: Stages 1-6 — Framework + **5개 필터** (`dbnd`, `arr`, `ts`, `dec`, **`sync`** 6 modes) + JSON 파서 + **CA 서버 wire-through** + **PVA 서버 wire-through**. `CA_PROTO_CREATE_CHAN`에서 `split_channel_name`으로 record path/JSON suffix 분리, `ChannelEntry.filter_suffix`에 stash. `CA_PROTO_EVENT_ADD`가 `parse_filter_chain`으로 chain 구성 → subscriber attach. **신규 sync 필터** (`crates/epics-base-rs/src/server/database/filters/sync.rs`): 6종 모드 모두 구현 — `before`/`first`/`last`/`after`/`while`/`unless`, upstream `db/std/filters/sync.c`와 시맨틱 동일. **dbState 모델**: 업스트림의 `dbStateGet/dbStateSet` 매칭 — 프로세스 전역 `DbStateRegistry` (OnceLock) 가 name → `Arc<DbState>` (AtomicBool) 매핑 유지. Trigger 레코드는 `db_state_registry().set(name, value)`로 상태 전환, 필터는 매 apply에서 `last_state`와 비교해 transition (0→1 / 1→0) 감지. Cache-and-emit 모드 (before/last)는 `Mutex<Option<event>>`에 latest 보관. JSON 두 가지 구문 지원: long form `{"sync":{"m":"after","s":"STATE"}}` + 모드-태그 shorthand `{"sync":{"after":"STATE"}}` (업스트림 chfTagString 등가). Alarm/property 이벤트는 446e0d4a 규칙으로 모든 모드에서 무조건 통과 + state tracker 미영향. **테스트 16종**: 6 모드 각 시맨틱 + 446e0d4a 모드별 verification + registry shared-state + 모드 keyword 파싱 + 파서 5종 (양·음 + 모든 6 모드 shorthand). **PVA wire-through (Stage 6)**: pvRequest의 `record._options._filter` 옵션을 JSON 필터 체인 문자열로 인식 — `record[_filter={"dec":{"n":3}}]` 구문으로 클라이언트가 CA와 동일한 JSON 체인 syntax를 PVA에서도 사용. INIT 경로에서 `monitor_filter_chain_json()` 헬퍼가 pvRequest의 `record._options._filter` 스칼라 스트링 추출, `parse_filter_chain()`으로 chain 빌드, `OpState.monitor_filters: Arc<FilterChain>`에 보관. 모니터 송신 루프(decoded path)에서 매 이벤트마다 `pv_field_to_filter_event()`로 PvField→FilteredMonitorEvent 변환 후 `chain.apply()`로 게이트. raw fast path는 `filters.is_empty()` 시에만 적격 — chain 활성 시 자동으로 decoded path로 강등되어 필터가 의미를 가짐. **추가 픽스**: `PvRequestExpr::encode()`가 type_desc만 보내던 latent bug — pvxs `to_wire_full(R, req)` 시맨틱 매칭으로 `to_pv_field()` 헬퍼 추가 + `encode_pv_field()` 호출로 record_options 스트링 값을 wire에 실제 송신. 통합 테스트 `server_side_filter_pva_dec_wire_through` — 클라이언트가 `record[_filter={"dec":{"n":3}}]` 보내고 N개 푸시 중 분수만 콜백에 도달함을 확인. **남은 작업**: ARR (배열 슬라이싱) 필터는 PvField mutation이 필요해 별도 PR; trigger PV process 경로에서 `db_state_registry().set(name, ...)` 자동 호출 (현재 수동 호출 필요).

---

## 2. IOC, 레코드 및 데이터베이스 (Records & Database)
- **`aai`, `aao`, `subArray` 등 배열 레코드 부재 (PR #162, #742)** — ✅ **DONE** `a02c310` (+follow-up): `ArrayKind` 열거형으로 `WaveformRecord` 공유, `aao`만 `can_device_write=true`. NORD 이벤트는 기존 waveform 경로에서 처리. **subArray INDX/MALM 슬라이싱 완료**: `WaveformRecord`에 `indx: i32`, `malm: i32` 추가 + `kind == SubArray`일 때만 `get_field/put_field`에서 노출. `set_val` 오버라이드가 SubArray에 대해 `source[INDX..INDX+NELM]` 슬라이스 (MALM>0이면 `min(source.len, MALM)`으로 추가 캡), 부족분은 NELM 크기 버퍼에 0-pad, NORD는 실제 복사 개수. INDX가 source 길이 초과 시 NORD=0. 5 단위 테스트 (정상 슬라이스 / out-of-range / partial tail zero-pad / MALM 캡 / non-subArray INDX·MALM 숨김).
- **`dbServerStats()` API 구현 지연 (PR #592)** — ✅ **DONE** `ac92e3e` (+follow-up): `ServerStats`의 모든 카운터 (channels_opened/closed + subscriptions_opened/closed + bytes_in/out) wired. **bytes_in/out**: `run_tcp_listener`가 `stats: Option<Arc<ServerStats>>`를 받아 `handle_client`로 전달, read마다 `bytes_in.fetch_add(n)` + 매 BufWriter flush 직전 `buffer().len()`을 캡처해 `bytes_out.fetch_add(...)`. **subscription counters**: `ServerConnectionEvent`에 `SubscriptionOpened`/`SubscriptionClosed` variants 추가, EVENT_ADD 성공 시 / EVENT_CANCEL / CLEAR_CHANNEL 시 / handle_client disconnect drain 시 emit. CaServer::run의 stats counter task가 두 variant를 받아 `subscriptions_opened_total`/`subscriptions_closed_total.fetch_add(1)`. 통합 테스트 2종: `server_stats_bytes_in_out_track_real_traffic` (실제 CaClient↔CaServer TCP 라운드트립), `server_stats_subscription_counters_track_camonitor_lifecycle` (camonitor open→drop으로 두 카운터 모두 ≥1 + opened==closed 등식).
- **`dbLoadTemplate`의 `EPICS_DB_INCLUDE_PATH` 지원 (PR #636)** — ⏭️ **ALREADY**: `iocsh/commands.rs:876`이 env var를 읽어 include path 리스트 구성.
- **잘못된 필드명에 대한 자동 제안 (PR #689)** — ⏭️ **ALREADY**: round 23 commit으로 `dbpf` typo suggestion 구현.
- **`dbPut` / `dbGet`의 16진수 및 8진수 문자열 지원 (PR #678)** — ✅ **DONE** `17210b4`: 기존 `parse_int`가 정수 타입 prefix를 처리했고, 본 commit으로 Double/Float도 `parse_string_to_f64`로 통합 (sign + 0x hex + 0-leading octal + 일반 decimal/exponent).
- **`asTrap` 내 `dbChannel` 노출 (PR #501)** — ⚠️ **N/A**: epics-rs는 generic asTrap 인터페이스 대신 `epics_ca_rs::audit::AuditLogger`를 사용하며, PV 이름·user·host·method·deny-reason 등 dbChannel-equivalent 컨텍스트를 이미 JSON으로 기록.
- **레코드 삭제 기능 (PR #505)** — ⏭️ **ALREADY**: `iocsh/commands.rs:24`에 `cmd_db_delete_record` 등록되어 있음.
- **`getenv` 디바이스 지원 (3.15.4)** — ✅ **DONE** `7ed3baf`: 신규 모듈 `server/builtin_devices/getenv.rs`. `IocBuilder::new()`/`IocApplication::new()`에서 `DTYP="getenv"`로 자동 등록. INP의 `@` prefix 처리.
- **출력 레코드의 `SIMM=RAW` 시뮬레이션 모드 (7.0.7)** — ✅ **DONE** `ac92e3e` (+follow-up): SIMM=2 명시 인식, RVAL-있는 레코드(ai/ao)에서 raw value path. **Conversion chain 정상 실행 fix**: pre-fix 입력 경로가 `put_field("RVAL", siol_val)` 다음에 `set_val(siol_val)`도 호출해 VAL을 raw count로 덮어쓰고 LINR/ESLO/EOFF/ASLO/AOFF 변환 체인을 우회했음. 이제 RVAL put + `record.process()` 호출로 정상 변환 실행, set_val은 제거. SIOL 값을 RVAL의 native DBR 타입(Long 등)으로 `convert_to()` 코어스 — pre-coerce 없이는 ai.put_field("RVAL", Double)가 TypeMismatch로 거부되어 rval=0 유지 → VAL=0\*ESLO+EOFF였음. RVAL 없는 레코드는 SIMM=YES와 동일 fallback (set_val 경로). 통합 테스트 `test_simm_raw_input_runs_conversion_chain` (LINR=1, ESLO=2, EOFF=10, raw=5 → VAL=20).
- **`longout` 레코드의 조건부 출력 `OOPT` 필드 (7.0.8)** — ✅ **DONE** `73b517c` (+follow-up): EpicsRecord derive를 manual `impl Record`로 교체, `should_output` override + 신규 trait method `on_output_complete` 추가, processing.rs의 device-write/soft-link 양 경로에 OOPT gate. **Follow-up**: PR #6c573b4 first-cycle bug — OOPT=1/4/5 transition modes가 default val=pval=0으로 첫 cycle 출력을 swallow하는 문제 fix. `first_output_done: bool` flag 도입, `compute_should_output()`이 첫 cycle은 항상 emit하도록 early-return; `on_output_complete()`에서 flag set. 신규 테스트 2종 (`oopt_on_change_first_cycle_forces_output`, `oopt_when_zero_first_cycle_forces_output`).

---

## 3. Shell (iocsh) 및 런타임 환경 (Runtime/Environment)
- **비대화형(non-interactive) `readline` 스킵 (PR #848)** — ✅ **DONE**: `IocShell::run_repl`이 `std::io::stdin().is_terminal()` 으로 분기 — TTY에서는 기존 rustyline 인터랙티브 경로, non-TTY(파이프/here-doc/`<script.cmd`)에서는 `run_repl_piped`가 `BufRead::lines()` 로 단순 읽기 + 프롬프트 출력 skip. 백그라운드 실행 시 captured stderr에 `epics>` 노이즈가 더 이상 섞이지 않음.
- **`iocshLoad` 명령어 미지원 (Issue #847)** — ✅ **DONE**: `IocShell::execute_line`이 `iocshLoad <path> [macros]` (space + C++ `iocshLoad("path","K=V,...")` paren form 양쪽 지원)을 인터셉트. `execute_script_with_macros`가 `db_loader::substitute_macros`로 라인별 `$(KEY)`/`${KEY}` 치환 후 재귀적으로 `execute_line` 디스패치. 빈 macros일 때는 substitution skip. 라인별 에러는 `execute_script`와 동일하게 다음 라인 진행 + 최종 Err 반환 (`iocshSetError` 등가). 테스트 5종: space form macro, paren form, no-macros, missing-path-error, per-line-error-propagate.
- **가용 CPU 수치 과다 보고 방지 API (PR #788)** — ⏭️ **ALREADY**: Rust `std::thread::available_parallelism()` (Rust 1.66+에서 Linux `sched_getaffinity` + cgroup 한도 반영)이 taskset/cgroup 제한 환경에서 정확한 가용 CPU 수를 반환. 사용처: `crates/epics-base-rs/src/server/iocsh/commands.rs:829`(`iocStats` CPU 수 보고), `crates/ad-plugins-rs/src/par_util.rs:50`(병렬 작업 자동 분할). C `sysconf(_SC_NPROCESSORS_ONLN)`이 affinity를 무시하던 문제가 std API 차원에서 해소.
- **`SIGTERM` / `SIGINT` 수신 시 `atExit` 정상 종료 절차 (PR #671)** — ✅ **DONE**: `IocApplication::run`이 protocol_runner future를 `tokio::select!`로 `signal::ctrl_c`(SIGINT) + `signal::unix::SignalKind::terminate`(SIGTERM)와 동시 polling. 시그널이 먼저 도착하면 runner future가 drop되어 모든 spawn된 task가 자동 정리되고 Ok(()) 반환. `biased` 분기로 runner가 먼저 완료된 경우 그 결과를 우선 propagate. CA(`ca_server.rs`)·PVA(`server_native/runtime.rs`)·tools(`procserv`)에 이미 있던 핸들러와 동등.
- **`iocsh` 내 `Ctrl+C` 처리 시 `stdin` 닫기 (PR #673)** — ⏸️ **DEFERRED**: upstream PR이 DRAFT 상태(미머지). 현재 Rust REPL은 rustyline의 표준 동작(`Ctrl+C` = 라인 취소 후 프롬프트 복귀, `Ctrl+D` = EOF로 종료)을 따름. PR이 머지되어 시맨틱이 확정되면 `ReadlineError::Interrupted` 경로 분기 결정.
- **`iocsh` 멀티라인 문자열 지원 (PR #603)** — ✅ **DONE**: `join_backslash_continuations` (`crates/epics-base-rs/src/server/iocsh/mod.rs`)이 라인 끝의 `\`+newline을 다음 라인과 합침. `execute_script` / `execute_script_with_macros` 양쪽에 적용. 시나리오 5종 (upstream `multiline-input.txt` 8라인 + 라인 번호 추적 + CRLF + EOF-without-newline + end-to-end). 제약: rustyline 기반 REPL에는 미적용 (대화형에서 `\`+enter 입력 시 continuation 없음 — 별도 readline editor 단계 필요).

---

## 4. `asyn` 드라이버 관련 (asyn-rs)
- **`asyn:READBACK` 연동 미완성 (PR #208, #60)** — ⏭️ **ALREADY**: round 6에서 `adapter.rs::asyn_readback` 플래그로 처리.
- **`UInt64` 인터페이스 파이프라인 (Issue #231)** — ⏭️ **ALREADY**: `asyn-rs/param.rs`에 `UInt64`/`UInt64Array` 타입 정의 완료.
- **Serial Port 드라이버 (PR #180)** — ⏭️ **ALREADY**: `asyn-rs/drivers/serial_port.rs::DrvAsynSerialPort` 구현.
- **FTDI 드라이버 (PR #88)** — ⏸️ **DEFERRED**: 미구현.
- **IP 서버 포트의 `Bind` 인터페이스 및 `SO_REUSEPORT` 지원 (PR #148, #109)** — ⏸️ **DEFERRED**: asyn IP 포트 드라이버의 특정 인터페이스 바인딩 옵션 미구현.
- **`lsi`, `lso`, `printf` 레코드에 대한 `asyn` 매핑 (PR #104)** — ⏸️ **DEFERRED**: `asyn-rs` 어댑터에서 LongString 파라미터 → 레코드 매핑 없음.
- **단순 평균치 장치 지원 (Issue #30)** — ⏸️ **DEFERRED**: `asynInt32Average`/`asynFloat64Average` 미구현.

---

## 5. 기타 세부 기능 및 엣지 케이스 (Minor Features & Edge Cases)
큰 골격 외에도, 과거 버그 픽스나 엣지 케이스 처리 중 아직 `epics-rs`에 완벽하게 반영되지 않았거나 교차 검증(Audit)이 필요한 자잘한 누락 사항들은 다음과 같습니다.

### 네트워킹 & 프로토콜 세부
- **DNS TTL 주기적 갱신 부재 (PR #862)** — ⏭️ **ALREADY**: `EPICS_CA_DNS_REFRESH_SECS` 타이머 + `AddrEntry::refresh_dns` 구현 (round 50).
- **네임서버 CA 프로토콜 강제 지정 (PR #621)** — ⏸️ **DEFERRED (upstream-DRAFT)**: name server를 거쳐 CA search 응답을 받을 때 UDP 버전이 name server의 것이므로 실제 server 버전 mismatch가 발생. PR은 compile-time `CA_V413` 강제값을 사용 — upstream PR이 DRAFT(검토 코멘트 다수). 머지 후 `epics-ca-rs` transport.rs의 version negotiation에 동등 옵션(아마도 env var) 추가.
- **다량 채널 검색(Mass-channel) 성능 튜닝 (Issue #372)** — 🔄 **PARTIAL**: AIMD search budget + 30-bucket cooperative tick 기반 구현되어 있으나 mass scenario 별도 검증 필요.
- **`caget` 반환 타입 단축 (PR #629)** — ⚠️ **N/A**: 원PR은 C `caget`의 `-d` argument parser에서 `DBR_INT` 문자열을 `DBR_SHORT`로 치환하는 패치(`dbr_text_to_type`이 `INT` suffix를 파싱하지 못하므로). `caget-rs`의 `-d/--dbr-type` 옵션은 현재 parity-only로 받기만 하고 채널 요청에 반영되지 않으므로(`bin/caget-rs.rs:233-235`) 변환 자체가 적용될 진입점 없음. `-d`를 실제로 wire-through할 때 같이 처리.
- **CA UDP 전송 오류 rate-limit (cae597d, c23012d)** — ⏭️ **ALREADY**: `client/search.rs::send_with_fanout`, `server/beacon.rs::run_beacon_emitter`에 per-destination first/change/recovery만 로그하는 dedup.
- **EPICS_CA_MCAST_TTL (3.16 f2a1834d)** — ✅ **DONE** `ae277d1`: `runtime::net::ca_mcast_ttl` + `AsyncUdpV4::set_multicast_ttl_v4` + CA 서버 beacon/UDP 응답기·클라이언트 search 소켓에 적용.
- **EPICS_IOC_IGNORE_SERVERS (6efe2924)** — ✅ **DONE** `8615bb4`: ADDR_LIST 파싱·SEARCH 응답·beacon 수신 3개 경로에서 quarantine IP 필터.
- **asLib DNS soft fallback (libcom 932e9f3)** — ✅ **DONE** `6862ef0`: ACF HAG 파싱 시 DNS 실패해도 abort 대신 literal 유지 + 가능한 IP 추가.

### 레코드 & 데이터베이스 세부

> 이 하위 섹션 항목 대부분은 점검·검증 작업이 본문 아래에서 별도로 다뤄집니다. 본 세션에서는 직접 다루지 않음 → **⏸️ DEFERRED**.

- **범용 `TOUT` (Timeout) 레코드 필드 부재 (PR #803)** — ⏸️ **DEFERRED (upstream-DRAFT)**: 새 dbCommon 필드 `TOUT`(초). 미처리 지속 시 `INVALID/TIMEOUT` 알람 자동 부착. upstream PR이 DRAFT — 필드 위치/시맨틱(pause 중 disable 등) 미확정. upstream 머지 후 dbCommon 필드 추가 + scan/process 경로에 watchdog 도입.
- **Soft Time Part 디바이스 지원 (PR #776)** — ⏸️ **DEFERRED (upstream-DRAFT)**: `ai DTYP="Soft Time Part"` + INP `@local.wday`/`@gm.hour` 등으로 시각의 일부(시/분/요일)를 fractional 값으로 노출(cron-like 트리거 용도). upstream PR DRAFT — 새 device support + INP 파서 형식 미확정. upstream 머지 후 std-rs(`device_support/time_of_day.rs` 옆)에 추가.
- **`bi` / `bo` 변환(Conversion) 로직 누락 (Issue #775)** — ⏸️ **DEFERRED (upstream design pending)**: enhancement 제안 — bi/bo에 `ZRVL`/`ONVL` 필드 추가로 mbbi/mbbo 스타일 임의 raw value 매핑 + MASK 의미 재정의. upstream Issue가 OPEN(question label) 상태이고 합의된 wire/field layout 없음. epics-rs 구현은 upstream이 새 필드 + MASK 시맨틱을 확정한 뒤 동기화.
- **상수 링크(Constant Link)의 오프셋 계산 버그 대조 (PR #467 / 1b46077)** — ⚠️ **N/A (eliminated)**: 원버그는 `lnkConst.c`의 `((char*)pbuffer)[*pnReq] = 0` one-past-end null terminator write로 `aai CHAR NELM=1` 1-byte heap overflow. Rust constant-link 데이터 복사는 `&str` / `Vec<u8>` slice의 checked indexing을 사용하므로 동일 패턴 자체가 컴파일러/런타임 차원에서 차단. `rust_verdict: eliminated`.
- **사용되지 않는 `INPx` 링크 파손 시 `calc` 레코드 중단 문제 (Issue #823)** — ⏭️ **ALREADY** (graceful by design): Rust multi-input fetch (`processing.rs` line ~270)는 `read_link_with_alarm`이 broken DB link에서 `get_pv` → `Err(ChannelNotFound)` → `.ok()` → `value=None`을 반환. `if let Some(value)` 분기를 통과하지 못해 해당 입력 슬롯은 default 그대로 유지되며, 알람 capture 분기는 ParsedLink::Db + Some(alarm) 모두 필요하므로 broken link에서는 alarm propagation도 일어나지 않음. CALC 식이 해당 입력을 참조하지 않으면 결과는 정상 계산 — Issue #823이 요청하는 동작을 자동 달성. (upstream issue OPEN이지만 Rust 구현은 이미 의도 부합.)
- **`mbboDirect`의 `B0..BF` 필드 ASL0 권한 조정 (PR #439)** — ⚠️ **N/A (design diff)**: 원PR은 `.dbd`의 `prompt(...)` ASL1 marker를 ASL0으로 바꿔 ACF 권한 게이트를 완화. epics-rs `FieldDesc`는 per-field ASL을 surface하지 않으며(필드 단위 ACF gating 미구현), 접근 제어는 `compute_access`/`AuditLogger` 경유 record-level + auth_method/authority 기반. 동등한 변경 지점이 없음.
- **`dbLoadRecords` 매크로 기본값 의미론 불일치 (PR #463)** — ⏭️ **ALREADY**: `db_loader::substitute_macros` (`crates/epics-base-rs/src/server/db_loader/mod.rs:268-291`)가 `$(KEY=default)` 분해 로직을 macros HashMap 비어있을 때도 동일하게 적용하므로 `dbLoadRecords("file.db","")`와 `dbLoadRecords("file.db")` (macros 인자 생략) 양쪽 다 default가 expansion됨. recursive default expansion + 따옴표 stripping까지 포함.
- **DB 파서의 알 수 없는 필드명 힌트 제공 (PR #434)** — ⏭️ ALREADY: round 23의 dbpf typo suggestion이 부분적으로 동일 기능 제공.
- **aSub 레코드의 상수 `INP*` 허용 여부 (Issue #284 / d47fa4c)** — ⏭️ **ALREADY**: `processing.rs` multi-input fetch (`PvDatabase::read_link_with_alarm`)이 `ParsedLink::Constant`에 대해 `link.constant_value()`를 직접 반환 (`links.rs:91`), `_ => (None, None)`로 polymorphic하게 처리. C가 `dbGetLink` 단일 진입점에서 constant에 에러 반환하던 패턴이 enum 분기로 자연스럽게 해소 — aSub `INPA..INPL`이 constant일 때도 fetch_values 등가 단계가 에러 없이 진행.
- **긴 문자열 `CALC$` 지원 이슈 (Issue #194)** — ⚠️ **N/A (out of scope)**: 원이슈는 C `caput -S test_calc.CALC$`(field name + `$` long-string suffix)가 일부 바이트만 작성되는 truncation 버그. `parse_pv_name` (`crates/epics-base-rs/src/server/database/mod.rs:22`)이 `$` 접미사를 별도 처리하지 않으며 `FIELD$` long-string access 자체가 미구현. truncation할 access 경로가 부재 — long-string `$` 접근이 추가될 때 이 항목 재검토.
- **`DBF_MENU` → `DBF_STRING` 변환 버그 픽스 대조 (Issue #183)** — ⏭️ **ALREADY**: `BiRecord::put_field("VAL", String)` / `MbbiRecord::put_field("VAL", String)` / `BoRecord::put_field("VAL", String)` 모두 ZNAM/ONAM/STATE_STR 문자열을 enum 인덱스로 매핑하는 분기 보유 (`crates/epics-base-rs/src/server/records/{bi,mbbi,bo}.rs`에 "epics-base PR/issue #183" 주석으로 명시).
- **`zero-length` (길이가 0인) 배열 지원 엣지 케이스 (7.0.5 / 5d808b7+3b3261c)** — ⏭️ **ALREADY**: upstream은 `S_db_emptyArray` 도입(5d808b7)했다가 호환성 문제로 revert(3b3261c) — 현재 C는 `S_db_badField`를 사용. Rust `CaError::InvalidValue`(`crates/epics-base-rs/src/error.rs:33` → ECA_BADTYPE 매핑)가 동일 의미로 일반화된 invalid-value 케이스를 처리하며, 빈 배열→스칼라 가드(12cfd41)가 이 경로에 적용됨. revert된 별도 `EmptyArray` variant는 의도적으로 추가하지 않음.
- **`compress` 레코드 개선 (7.0.8)** — ✅ **DONE**: `PBUF` 필드 추가 + `get_field("VAL")` 분기 (`crates/epics-base-rs/src/server/records/compress.rs`). PBUF=YES 시 `nuse < nsam` 동안 valid prefix만 노출 (`val[..nuse]`), PBUF=NO/full 시 historic 전체 NSAM 벡터 노출. menu string("YES"/"NO") put도 지원. **N-to-1 partial buffer (PR #84f4771)**: 신규 `push_array(&[f64])` 메서드 — 배열 입력을 chunk 단위로 처리하고, 입력이 mid-chunk으로 끝났을 때 PBUF=YES면 partial accum을 즉시 flush, PBUF=NO면 다음 호출까지 유지. 단일 element `push_value`와 `flush_accum` 헬퍼로 코드 공유. 7 unit tests (기존 PBUF + N-to-1 partial tail emit/defer + circular buffer pass-through).

### Shell & 시스템 코어 세부

> 본 세션에서 직접 다루지 않음 → **⏸️ DEFERRED**.

- **`iocsh` 다중 후행 줄바꿈(trailing newlines) 트리밍 (PR #371)** — ⏭️ **ALREADY**: Rust `String::trim()`이 연속 줄바꿈 포함 모든 leading/trailing whitespace를 제거. `iocsh/mod.rs:38/159/229/231` 모두 명령 라인을 `trim()` 후 처리.
- **`initHookRegister` 멱등성 보장 로직 (PR #594 / 13d6ca5)** — ⚠️ **N/A (design diff)**: Rust 측은 builder pattern으로 per-`IocApplication`의 `after_init_hooks: Vec<Box<dyn FnOnce>>` (`crates/epics-base-rs/src/server/ioc_app.rs:68`)만 존재. C의 전역 `functionList` linked list와 달리 module-static-init/iocsh 양쪽에서 동일 fn pointer가 누적되는 시나리오가 구조적으로 발생하지 않음. 보너스: closure는 비교 불가능하므로 dedup도 의미 없음 (사용자가 의도적으로 두 번 등록한 경우 그대로 두 번 실행).
- **새로운 문자열 유틸리티 부재 (7.0.5/7.0.6)** — ⏭️ **ALREADY**: 두 유틸 모두 epics-rs 내에 functional analog 존재. `epicsStrnGlobMatch` ↔ `glob_match` (`crates/epics-base-rs/src/server/iocsh/commands.rs:750`, `dbglob` 명령에서 사용). `epicsStrSimilarity` ↔ `edit_distance_short` (commands.rs, PR #689 typo suggestion에서 사용). C 전역 라이브러리 API와 달리 호출 지점에 가까운 모듈에 위치.
- **빈 인스턴스의 `dbLoadTemplate` vs `msi` 파서 불일치 (Issue #666)** — ⚠️ **N/A (out of scope)**: `dbLoadTemplate` 다중 인스턴스 로더 + `msi` 도구 자체가 epics-rs 미구현 (현재 `dbLoadRecords` per-call macros만 존재). 두 도구의 파서 불일치도 자동 부재. 추후 substitution file 로더가 추가될 때 이 항목 재검토.
- **서버 필터 프레임워크 셧다운 안전성 (Issue #643)** — ⚠️ N/A: 서버 측 필터 프레임워크가 미구현이므로 그 셧다운 안전성 이슈도 자동 무관.

### Asyn 모듈 세부

> 본 세션에서 직접 다루지 않음 → **⏸️ DEFERRED**.

- **직렬 포트 `Auto serial break` 기능 (PR #188)** — ⏸️ DEFERRED.
- **`ASYN_TRACE_STATE` 마스크 비트 (PR #67)** — ⏸️ DEFERRED.
- **`asynMask`의 시프트 파라미터 (Issue #166)** — ⏸️ DEFERRED.
- **`setStringParam` NULL 포인터 안전성 (Issue #146)** — ⏭️ ALREADY: 러스트의 `Option<&str>` 모델 + 타입 시스템으로 구조적 방어. NULL deref 자체 발생 불가.
- **EOS(End-of-String) 설정자 블록 문제 (Issue #103)** — ⏸️ DEFERRED.
- **장치 드라이버로의 파라미터 변경 알림(Notification) 방향성 버그 대조 (Issue #46)** — ⏸️ DEFERRED.
- **`drvAsynIPPort` 읽기 타임아웃 시 연결 종료 옵션 (PR #6)** — ⏸️ DEFERRED.
- **`asynSetTrace*Mask`의 문자열 옵션 파싱 (PR #76)** — ⏸️ DEFERRED.

---

## 6. 로드맵 상의 의도적 제외 사항(By-Design Gaps) 및 코드베이스 TODO
과거 커밋이나 이슈가 아닌, `epics-rs`의 아키텍처 철학(`ROADMAP.md`) 및 코드베이스 내부의 `TODO` 주석을 통해 파악된 마지막 미구현/제외 항목들입니다.

### 의도적 제외 사항 (Out-of-Scope)
C++ `epics-base`에는 존재하지만 러스트 생태계의 특성상 **의도적으로 구현하지 않기로 한(By-Design)** 항목들입니다.
- **RTEMS 및 VxWorks 운영체제 지원**: `epics-rs`는 Linux(및 PREEMPT_RT), macOS, Windows 등 Tier-1/2 호스트 OS에 집중하며, 임베디드 실시간 OS 지원은 아예 스코프에서 제외되었습니다. (해당 용도는 C++ `pvxs` 권장)
- **C/C++ 호환 OSI (Operating System Independent) 레이어**: `epics-base`처럼 자체 OS 추상화 레이어를 만들지 않고, 전적으로 러스트의 `tokio` 생태계 및 표준 라이브러리에 의존합니다.
- **서브 마이크로초(Sub-microsecond) 단위의 Hard-RT**: 리눅스 PREEMPT_RT 기반의 1ms 미만 지터(Jitter) 억제에 집중하며, 마이크로컨트롤러 수준의 펌웨어형 실시간성은 지원하지 않습니다.

### 소스코드 내 잔여 TODO (Internal Technical Debt)
- **PVA 서버의 NIC(네트워크 인터페이스) 동적 갱신**: 서버가 구동 중일 때 새 랜카드(NIC)가 추가되거나 IP가 변경되면, 이를 실시간으로 재조회(re-resolve)하여 바인딩하는 로직이 아직 비어있습니다. (`crates/epics-pva-rs/doc/05-server.md` TODO)
- **IOC 초기화 시 Two-pass 의존성 해결**: 레코드 간의 초기화 의존성이 복잡하게 얽힐 경우를 대비한 '글로벌 투패스(Global two-pass)' 리팩토링이 보류되어 있습니다. (`crates/epics-base-rs/src/server/iocsh/commands.rs` TODO)
- **AreaDetector 플러그인 Stub**: `ad-plugins-rs` 모듈 내에 구현되지 않은 플러그인 타입들이 껍데기(Passthrough No-op stub) 형태로만 존재합니다.

---

## 7. 커밋 전수조사 결과 — `epics-rs`에서 점검이 필요한 항목들
**범위**: `epics-base` 저장소의 3.15 이후 커밋 총 1,370개를 카테고리별로 전수조사.  
KEEP 판정된 422개 커밋을 분류하여, 러스트 채택으로 자동 해결된 것과 아직 직접 반영이 필요한 것을 분리합니다.

> **본 세션 상태**: Section 7~10의 개별 항목은 별도 PR 단위로 분리되어 다뤄지므로, 본 세션에서는 일괄 **⏸️ DEFERRED** (이미 위 1~4 섹션에서 본 세션이 다룬 항목은 거기서 ✅/⏭️로 표시되어 있음). 아래에는 본 세션과 직접 관련된 항목만 status를 명시.

---

### 7-A. 와이어 프로토콜 / DBR 인코딩 (Wire-Protocol, 32건 → 점검 필요)
- **`DBE_PROPERTY` 이벤트 중복 발송 방지** (`faac1df1`, 2024) — ✅ **DONE**: 신규 `RecordInstance::notify_field_written_if_changed(&str, Option<&EpicsValue>)`가 metadata 필드인지 확인 후 prev vs current 비교, 변경된 경우에만 `invalidate_metadata_cache` 호출. `put_pv`/`put_pv_and_post_with_origin`/`put_record_field_from_ca`/`put_pv_no_process` 4개 call site 모두 pre-put `get_field`로 prev capture 후 새 메소드 사용. C `propertyUpdate && memcmp != 0` 분기와 동일 의미. 테스트 3종 (no-op skip, real change invalidate, non-metadata skip).
- **`DBE_PROPERTY` → `DBE_VALUE` 순서 보장** (`b7cc33c3`, 2024) — ⚠️ **N/A (design diff)**: C는 dbPut에서 metadata 변경을 `DBE_PROPERTY` 별도 이벤트로 먼저, 값 변경을 `DBE_VALUE` 이벤트로 뒤에 emission하며 client 측 formatting이 새 metadata를 반영하도록 함. epics-rs는 모든 snapshot에 metadata(`MetadataSnapshot::{display,control,enums}`)를 캐시된 형태로 항상 포함시켜 함께 보내므로 `DBE_PROPERTY` vs `DBE_VALUE` 분리 자체가 없음 — 순서 보장 대상이 부재.
- **`mbbi`/`mbbo`의 `DBE_PROPERTY` 누락 버그** (`9e7cd24`, 2024) — ⚠️ **N/A (design diff)**: C는 mbbi/mbbo가 ZRST/ONST/... state string 변경 시 `DBE_PROPERTY`를 발송하지 않던 버그. Rust는 metadata snapshot에 state string을 매번 포함시키므로 별도 PROPERTY 이벤트 발송 자체가 없으며 누락 패턴도 부재. state-string 변경 후 `notify_field_written_if_changed`(faac1df1 보강)가 cache invalidate → 다음 snapshot에 새 state string 반영.
- **빈 배열(length=0) `caput` 시 DBR 오프셋 오계산** (`8cc20393`, 2020) — ⏭️ **ALREADY**: C 매크로 `dbr_size_n`의 `(COUNT)-1` 언더플로 버그(unsigned)는 Rust `DbFieldType::buffer_size(count) = element_size() * count` 직접 곱셈으로 발생 불가. `count=0` → `value_size=0` (`crates/epics-base-rs/src/types/dbr.rs:147,167`). `dbr_buffer_size`도 동일.
- **빈 배열 `caput` 시 스칼라에 `INVALID_ALARM` 설정** (`12cfd418`, 2020) — ⏭️ **ALREADY** (semantic diff): `put_pv` (`crates/epics-base-rs/src/server/database/field_io.rs:83-92`)가 commit 12cfd41 hash까지 명시한 가드로 `value.is_empty_array() && target_is_scalar` 케이스를 `CaError::InvalidValue` 반환으로 reject. C의 LINK_ALARM/INVALID_ALARM 세트 대신 Err 전파로 fail-fast — converter는 호출되지 않으며 garbage value 작성도 차단됨. 알람 필드 변경은 발생하지 않으므로 stat/sevr monitor 채널은 영향 없음 (C와 미세 차이).
- **`dbGet`으로 빈 배열을 스칼라로 읽을 때 크래시** (`39c8d561`, 2020) — ⏭️ **ALREADY** (defense-in-depth): C UB는 `memcpy(dst, src, 0)` + pointer 산술. Rust 측은 `EpicsValue::*Array(Vec<...>)`로 빈 Vec 자체가 안전한 값 (`.iter()`/슬라이스 모두 well-defined). 또한 빈 array→scalar 경로는 12cfd41 가드 (`field_io.rs:83-92`)가 `CaError::InvalidValue`로 차단. dbGet→스칼라 코드 패스에서 패닉/UB 발생 경로 없음.
- **`UTAG` uint64 타입 필드 전파** (`b94afaa0`, 2020) — ⚠️ **N/A** (의도적): `epics-rs`의 `snapshot.user_tag: i32`는 PVA Normative `time_t.userTag = int` 스펙과 일치하는 wire-correct 표현. upstream의 internal uint64는 CA-level UTAG 노출 + db_field_log 전파(둘 다 미구현)에 의미. CA-level UTAG가 필요해질 때 i64로 승격 + PVA encode에서 truncate-with-warning.
- **`amsg`/`utag`의 `dbGet()` 옵션 통로 분리** (`bd3ecf1c`, 2021) — ⚠️ **N/A (design diff)**: C는 `dbGet`에 별도 옵션 비트로 AMSG/UTAG를 전달. Rust는 PVA serialization 시 `Snapshot { amsg, user_tag, ... }` 구조체로 한 번에 전달하므로 별도 통로 분리 필요 없음 — wire field로 직렬화 위치만 결정.
- **`db_field_log::mask` 필드** (`235f8ed2`, 2020) — ⚠️ **N/A (depends on filter framework)**: 원커밋은 `db_field_log` 구조체에 `mask: u8` 필드를 추가해 filter chain이 DBE_PROPERTY vs DBE_VALUE 구분 가능하게 함. epics-rs는 server-side filter framework 자체가 deferred (섹션 1 참조) — filter chain이 받을 event log struct이 부재. 필터 구현 시 `EventMask` 필드를 처음부터 포함하도록 설계.
- **CA 서버 프로토콜 버전 클라이언트 노출** (`d7635413`, 2025, PR #711) — ⏭️ **ALREADY**: 섹션 1 참조 (transport.rs server_minor_version 분기).
- **`SOCK_CLOEXEC` 사용 + `accept4()`** (`cf3173b6`, 2021) — ⏭️ **ALREADY**: Tokio가 내부에서 처리, `epics-rs` 직접 점검 불필요.
- **`IPPORT_USERRESERVED` 포트 상수 정의** (`cd0e6a4f`/`0cae0db`, 2020-2021) — ⚠️ **N/A (eliminated)**: musl libc 헤더 호환 shim. Rust `std::net`/tokio는 `IPPORT_USERRESERVED`를 사용하지 않으며 epics-ca-rs는 하드코딩 상수/환경변수로 포트 선택. `rust_verdict: eliminated`.
- **`16진수/8진수 문자열 dbPut/dbGet 지원`** (`88bfd6f3`, 2025, PR #678) — ✅ **DONE** `17210b4` (섹션 2 항목과 동일).
- **`bi` 레코드 소프트 채널에서 `MASK` 비트 사용** (`f2fe9d12`, 2023) — ✅ **DONE** `97300ce`: 섹션 1/2 보강. `Record::accepts_raw_soft_input()` + `apply_raw_input` opt-in trait + BiRecord override (RVAL routing + MASK AND) + processing.rs Raw Soft Channel 분기. 테스트 4종 (unit 3 + integration 1).

---

### 7-B. 레코드/DB 경계값 및 배열 (Bounds, 56건 → 핵심 점검 항목)
- **`constant link` 오프셋 오프바이원(off-by-one) 버그** (`1b460770`, 2024) — ⚠️ **N/A (eliminated)**: 섹션 5 보강 참조 (Rust slice indexing이 one-past-end write를 차단).
- **`aai` 레코드의 pass-1 디바이스 초기화 지원** (`1c566e21`/`6754404d`, 2021): `aai`가 pass-1에서 디바이스를 초기화해야 하는 순서 의무. → 섹션 2의 `aai` 구현 항목에 포함.
- **배열 레코드의 `BPTR` 필드 런타임 변경 지원** (`2340c6e6`, 2021): 배열 버퍼 포인터를 런타임에 변경하는 기능.
- **`compress` 레코드 평균(average) 알고리즘 버그 수정** (`11a4bed9`, 2022): 단일 입력 데이터 경로 및 스칼라 평균 처리 버그. → 섹션 5의 기존 항목에 포함.
- **`lsi`/`lso` 레코드의 `SIZV` 필드 크기 계산 버그** (`4966baf4`, 2024): 긴 문자열 레코드의 버퍼 크기 필드가 잘못 계산되는 버그.
- **`arrRecord`의 `cvt_dbaddr()` 동작 통일** (`eeb198db`, 2020): `arrRecord`의 주소 변환 동작을 `waveform`/`aai`와 일치시켜야 함.
- **`dbConstAddLink`의 DBR 타입 경계 검사** (`552b2d17`, 2021): 링크 상수 처리 시 DBR 타입 인덱스를 경계 검사해야 함.
- **호스트명 최대 길이 제한 제거** (`87acb98d`, 2022): CA 주소 리스트 파싱에서 짧은 고정 버퍼로 호스트명이 잘리는 버그.
- **`iocinf.cpp` 호스트명 버퍼 오버플로** (`a8e8d22c`, 2022): 32바이트를 초과하는 호스트명이 CA 클라이언트에서 잘리는 버그.
- **`postfix()` 함수의 널 포인터 역참조** (`60fa2d31`, 2023): calc 엔진의 후위 변환기에서 잘못된 입력 시 크래시.
- **`dbEvent` 잔여 이벤트 카운트(`eventsRemaining`) 오계산** (`e1c1bb8b`, 2023): 이벤트 큐에서 남은 이벤트 수 계산 오류로 조기 종료.
- **`callbackSetQueueSize` 상한 검사** (`baa4cb54`, 2025) — ⚠️ **N/A (design diff)**: epics-rs는 C의 `callback.c` 큐 시스템을 사용하지 않고 tokio 런타임의 `mpsc::channel` / spawn 으로 콜백 워크를 처리. `callbackSetQueueSize` 등가 API 자체가 없음. 음수/0 큐 사이즈 입력 검증이 필요한 진입점이 부재.
- **`CHAR` 배열 출력 시 비출력 문자 이스케이프** (`dc70dfd6`, 2022) — ✅ **DONE**: `cmd_dbgf`가 `EpicsValue::CharArray` 케이스에서 신규 `escape_char_array_for_dbgf` 헬퍼로 C 스타일 escape 후 큰따옴표 wrap (`"..."`). short form: `\n` `\t` `\r` `\\` `\"` `\a` `\b` `\f` `\v`, 그 외 non-printable 및 high-bit (0x7f..=0xff)는 `\xNN`. 다른 EpicsValue 타입은 기존 Display 그대로. Unit 테스트 3종.

---

### 7-C. 런타임 수명주기 / 셧다운 (Lifecycle, 112건 → 핵심 점검 항목)
- **`CA Repeater`를 프로세스 실행 실패 시 스레드로 폴백** (`08b741ed`, 2021): `caRepeater` 실행 파일을 실행할 수 없을 때 내부 스레드로 대체 실행하는 로직. `epics-ca-rs`의 Repeater 시작 전략 확인.
- **`caRepeater`에 `-d` 디버그 옵션 추가** (`e2717521`, 2026): 커맨드라인 디버그 플래그 지원. → 섹션 1의 관련 항목(debug flag) 확인.
- **`iocsh` 명령어에 `iocshSetError()` 전파** (`144f9756`, 2024) — ✅ **DONE**: `IocShell::execute_script` / `execute_script_with_macros`가 라인별 `Err`를 `last_err`로 캡처하여 스크립트 종료 시 종합 Err 반환 (=`iocshSetError` 의 비-제로 exit code 등가). 본 commit에서 `dbLoadRecords`가 add_record 거부 시 `Ok(Continue)`로 swallow하던 케이스(`commands.rs:1000-1002`)를 `Err(e)` 반환으로 수정 + duplicate name regression 테스트 추가.
- **`iocsh` 인자 파싱 버그 수정** (`3dbc9ea2`, 2023) — ⏭️ **ALREADY**: 원버그는 `char quote = EOF (-1)` 센티넬이 VxWorks의 unsigned char에서 `0xFF`로 wrap되어 입력의 0xFF 바이트와 충돌. Rust tokenizer (`crates/epics-base-rs/src/server/iocsh/registry.rs`)는 `let mut in_quotes: bool` 로 양자 상태를 유지하므로 sentinel 충돌 가능성 자체가 없음. 추가로 `find_closing_paren`/`split_comma_args`/`split_space_args` 3곳 모두 동일 패턴.
- **`casStatsFetch()` RSRV 미초기화 시 안전성** (`7a6e11ca`, 2026) — ⚠️ **N/A**: C는 전역 `clientQlock` / `rsrvCurrentClient`가 미초기화 NULL 상태에서 stats 조회 시 NULL deref. Rust `ServerStats`는 `Arc<ServerStats>` (`crates/epics-ca-rs/src/server/ca_server.rs:337+`) 이며 `Default::default()`로 항상 0 초기화 + atomic 카운터 사용 — "미초기화 NULL"이라는 상태 자체가 존재하지 않음. RSRV disabled 시에도 `Arc<ServerStats>`는 valid한 0-stats를 반환.
- **`dbGet`의 루프-안전 래퍼** (`dac620a7`, 2024) — ⚠️ **N/A (design diff)**: C dac620a7는 `dbDbGetControlLimits/GraphicLimits/AlarmLimits`가 같은 필드를 가리키는 link 따라가다가 무한 재귀에 빠지는 케이스를 `DBLINK_FLAG_VISITED` 플래그로 차단. epics-rs는 메타데이터(HOPR/LOPR/DRVH/DRVL/alarm limits)를 별도 link traversal로 가져오지 않고 record 필드에서 직접 읽기 때문에 재귀 경로 자체가 부재. process chain 재귀는 `visited: HashSet<String>`로 별도 보호됨.
- **`NAMSG` 알람 문자열 필드를 `NSTAT`/`NSEV`와 함께 초기화** (`8483ff95`, 2024) — ⏭️ **ALREADY**: `rec_gbl_reset_alarms`(`crates/epics-base-rs/src/server/recgbl.rs:121`)가 `common.amsg = std::mem::take(&mut common.namsg)`로 promote 직후 namsg를 자동 클리어. `reset_alarms_transfers_amsg_and_clears_namsg` 테스트로 회귀 방어.
- **`lset::getAlarmMsg()` API** (`5143c71a`, 2020): 링크 세트(link set)에서 알람 메시지를 직접 읽어오는 새 API.
- **빈 문자열 링크를 `unset`과 동일하게 처리** (`3b484f58`, 2023) — ⏭️ **ALREADY**: `parse_link_v2` (`crates/epics-base-rs/src/server/record/link.rs:233-235`)가 `s.is_empty()` 케이스를 `ParsedLink::None`으로 반환. JSON form `{const:""}`도 `try_parse_json_link:143-144`에서 동일하게 `None` 처리.
- **`FIFO 스케줄링`을 환경 변수로 비활성화** (`862272d6`, 2025) — ⚠️ **N/A (design diff)**: 원 commit은 `EPICS_ALLOW_POSIX_THREAD_PRIORITY_SCHEDULING=NO`로 C `epicsThread`의 `SCHED_FIFO` 활성화를 끄는 기능. Rust 측은 tokio runtime + std `thread::Builder` 사용으로 SCHED_FIFO/sched_setscheduler를 호출하지 않음 — RT 스케줄링 활성화 자체가 없어 비활성화할 대상도 없음. ROADMAP Phase 1의 RT 잠재 도입 시 동등 env var 추가.
- **`memlock()` 옵트아웃** (`0916cf98`, 2025) — ⚠️ **N/A (design diff)**: FIFO 비활성화 시 `mlockall()`도 건너뛰는 C 코드. Rust는 `mlockall` 자체를 호출하지 않음 (RT 메모리 잠금 미사용) — 건너뛸 호출이 부재.
- **`aSub` 레코드의 상수 `INP*` 링크 지원** (`d47fa4ca`, 2022, Issue #284) — ⏭️ **ALREADY**: 섹션 5 보강 참조. 핵심: `read_link_with_alarm`의 `ParsedLink::Constant(_)` arm이 constant value를 직접 반환하므로 dbGetLink 등가 호출이 발생하지 않음.
- **`dbLoadRecords()` 오류 메시지 중복 출력 방지** (`9af7fb3`, 2025) — ⏭️ **ALREADY**: 원버그는 C `softMain.cpp`가 `dbLoadRecords` 내부 에러 메시지 + 자체 wrapper 메시지를 둘 다 출력. Rust `cmd_db_load_records` (`iocsh/commands.rs`)는 단일 `ctx.println(&e)` 후 `Err(e)` 전파만 수행하며 `softioc-rs`도 자체 wrapper 메시지를 추가하지 않음. 중복 출력 경로 자체가 부재.
- **`dbReadDatabaseFP()` 파일 닫기 보장** (`a6779df2`, 2022) — ⚠️ **N/A (eliminated)**: Rust `std::fs::File`의 `Drop`이 자동으로 `close()` 보장. `BufReader<File>` 등 모든 파일 래퍼 동일. `rust_verdict: eliminated`.
- **`logClient` 연결 끊김 시 미전송 메시지 재전송 시도** (`0a3427c8`, 2019) — ⚠️ **N/A (design diff)**: epics-rs는 C의 `logClient.c` TCP forwarder를 사용하지 않고 `tracing` crate로 구조화된 로깅을 처리. 로그 서버 reconnect 시 retransmit이 필요한 송신 버퍼 자체가 부재. EPICS log server protocol 지원이 필요해질 때 buffer-preserve 정신을 적용.
- **알람 메시지 필드(`AMSG`) 및 타임 태그 필드(`UTAG`) 추가** (`892a361d`/`b94afaa0`, 2020) — ⏭️ **ALREADY (partial design)**: AMSG (+ NAMSG promote-pair)는 `CommonFields::amsg`/`namsg: String`(`crates/epics-base-rs/src/server/record/common_fields.rs:16,22`) 으로 구현되어 알람 발생/리셋 경로에서 정상 전파. UTAG는 PVA snapshot에 `user_tag: i32`로 노출(섹션 7-A의 b94afaa0 N/A 보강 참조) — wire-correct i32 형태 유지, 64bit 승격은 CA-level UTAG 필요 시 별도 PR.
- **`dbChannel` 기반 링크 (DBADDR → dbChannel 교체)** (`b1f44592`, 2020) — ⚠️ **N/A (design diff)**: C는 link 내부 주소를 `DBADDR`에서 `dbChannel`로 교체하여 필터 메타데이터를 동반시킴. epics-rs는 `ParsedLink::Db(DbLink { record, field, policy, monitor_switch })` 구조로 시작부터 record-name + field-name + 필터 메타데이터를 단일 enum variant에 담음 — DBADDR vs dbChannel 분리 자체가 부재. 서버 필터 프레임워크가 추후 도입되면 `DbLink`에 필터 chain 필드 추가로 자연스럽게 확장.

---

### 7-D. 타이밍 / 타임아웃 (Timeout, 15건 → 검토 필요)
- **타이머 조기 만료(Early expiry) 버그** (`01360b2a`, 2022): 비 RTOS 환경에서 타이머가 예정보다 일찍 발화하는 버그. `tokio` 타이머가 대체하지만, CA 검색 타이머(`SearchTimer`) 로직 확인.
- **NaN/Overflow 타임아웃 값 처리** (`1655d68e`, 2022) — ✅ **DONE (analog)**: 원커밋은 RTEMS osdEvent 한정 (Rust 비대상)이지만 동일 정신: `epics_ca_rs::cli::timeout_duration` (`crates/epics-ca-rs/src/cli.rs`)이 NaN/±Inf/0/음수를 `DEFAULT_CLI_TIMEOUT_SECS=1.0`으로 클램프해 `Duration::from_secs_f64` panic을 차단. `env_default_timeout`도 같은 가드. 4개 CA CLI(caget/caput/cainfo/camonitor) 모두 `timeout_duration` 경유. PVA 측은 `epics_pva_rs::cli::timeout_duration` (default 5.0s) 추가, `pvcall-rs`에 적용. `pvlist-rs`는 `0`=wait-forever 의미를 보존하기 위해 `is_finite() && > 0.0` 가드만 적용 (Inf/NaN도 wait-forever). 테스트 5종.
- **`EPICS_CLI_TIMEOUT` 환경 변수** (`1d056c6f`, 2022) — ⏭️ **ALREADY**: `epics_ca_rs::cli::env_default_timeout` (`crates/epics-ca-rs/src/cli.rs:10`)이 EPICS_CLI_TIMEOUT 환경변수를 읽어 unset/unparseable 시 1.0s fallback. `caget`/`caput`/`camonitor`/`cainfo` 4개 binary 모두 `.unwrap_or_else(env_default_timeout)` 패턴으로 적용. clap의 `-w` parse 실패 시 즉시 종료(C의 silent-revert와 달리 안전).
- **단조시간(Monotonic Clock) 기반 CA 타임아웃 통일** (`f1cbe93b`, 2020): CA 내부 타이머(`tcpiiu`, `searchTimer`)를 모두 단조 시계 기반으로 통일하는 작업. `tokio`의 `Instant` 사용으로 대체되나, 동일한 단조 보장 여부 확인.
- **macOS 단조 시계 해상도 버그** (`3506d115`, 2020): 최신 macOS에서 `clock_gettime`의 오버헤드를 줄이기 위한 최적화(macOS Tier-2 개발 플랫폼이므로 확인 필요).

---

### 7-E. 동시성 / 데이터 레이스 (Race, 46건 → Rust로 대부분 해결됨)
아래 C++ 커밋들은 **Rust의 소유권 모델, `Arc<Mutex<>>`, `tokio` 비동기 런타임**으로 인해 구조적으로 해결된 사례들입니다. 단, 일부는 논리적 동시성 이슈이므로 교차 검증이 필요합니다.

- **`concurrent db_cancel_event()` 데드락** (`9f868a10`, 2023): 이벤트 취소 중 다른 스레드와 데드락. Rust의 잠금 순서(`LockOrder`) 관리로 검토 필요.
- **`db_create_read_log`/`dbChannelGetField` 잠금 누락** (`9f788996`, 2023): 레코드 잠금(Lock) 없이 읽기 로그를 생성하는 버그. `epics-rs`의 레코드 처리 경로 동기화 모델 확인.
- **`epicsThreadOnce()` 경쟁 조건** (`5507646c`, 2023): → Rust의 `std::sync::Once`로 원천 해결됨.
- **`ipAddrToAsciiGlobal` 공유 스크래치 버퍼 레이스** (`82338657`, 2023): 비동기 DNS 조회에서 스크래치 버퍼가 공유되는 레이스. Tokio의 비동기 DNS 및 채널 분리로 해결됨.
- **`epicsMessageQueue` 스레드 노드 미초기화** (`a7a56912`, 2023): → Tokio `mpsc::channel`로 대체.
- **`dbCaSync()` 수정** (`e9e576f4`, 2021): CA 링크 동기화 함수의 경쟁 조건. `epics-rs`의 CA 링크 갱신 동기화 모델 확인.
- **`CLOCK_MONOTONIC_RAW` 제거** (`597393a8`, 2019): 플랫폼에 따라 단조성이 보장되지 않는 클럭 소스 제거. Tokio `Instant`로 해결됨.
- **우선순위 역전 뮤텍스(PI Mutex)** (`5a8b6e41`, 2020): `epicsMutex`에 우선순위 상속 지원 추가. Linux RT 목표에 맞게 `tokio`의 `Mutex`와 POSIX PI mutex 조합 검토 필요 (ROADMAP Phase 1 관련).

---

### 7-F. 흐름 제어 / 큐 (Flow-Control, 7건)
- **`dbnd` 필터의 알람/프로퍼티 이벤트 통과** (`446e0d4a`, 2021): 데드밴드(deadband) 필터가 `DBE_ALARM` 및 `DBE_PROPERTY` 이벤트는 항상 통과시켜야 함. `epics-rs` 서버 필터 구현 시 반드시 반영 필요.
- **`dbEvent` 큐 사이즈 조정** (`c8e5deca`, 2019): 이벤트 큐 크기 기본값 변경. `epics-rs`의 이벤트 큐 버퍼 정책 검토.
- **`callbackParallelThreads` 비율(%) 지정 지원** (`fe39a007`, 2026) — ⚠️ **N/A (design diff)**: epics-rs는 C의 `callback.c` 큐 시스템과 그 `callbackParallelThreads` iocsh 명령을 갖지 않음 (tokio 런타임이 callback work를 처리). 백분율 인자 파싱이 적용될 진입점 자체가 부재. tokio worker thread 수는 별도 `tokio::runtime::Builder::worker_threads` 설정으로 제어, 백분율 입력이 필요해질 때 iocsh 명령 신설 시 동등하게 처리.
- **CPU 과다 보고 방지** (`556de06f`, 2026): → 섹션 3의 기존 항목과 동일 (PR #788).
- **필터 내 `dbGet` 통과 경로** (`17a8dbc2`, 2020): `dbDbGetValue()` 내에서 채널 필터를 거치는 흐름 경로. `epics-rs`의 서버 필터 컨텍스트 처리 확인.

---

### 7-G. 러스트 채택으로 구조적 해결 (Equivalent — 정보용)
아래는 C++에서 수백~수천 줄의 패치로 해결했으나 `epics-rs`에서는 언어/프레임워크 특성으로 원천 해결된 사례들입니다:

| C++ 버그 유형 | 관련 커밋 수 | Rust 해결 메커니즘 |
|---|---|---|
| `fdManager` poll/select/FD 누수 | ~30건 | `tokio` epoll/kqueue/IOCP 추상화 |
| 스레드 종료 시 소켓/핸들 누수 | ~25건 | RAII + `Drop` 트레이트 |
| `epicsMutex` 초기화 레이스 | ~10건 | `std::sync::Once` / `OnceLock` |
| `gethostbyname` thread-unsafe | ~5건 | `std::net::ToSocketAddrs` |
| `volatile` 오용 / 원자성 누락 | ~8건 | `std::sync::atomic::AtomicXxx` |
| 전역 생성자 순서 의존성 | ~6건 | Rust 모듈 초기화 보장 |
| `malloc`/`free` 불일치 | ~15건 | `Box<T>` / `Arc<T>` 자동 해제 |
| `sprintf` 버퍼 오버플로 | ~8건 | 문자열 포맷 시 경계 자동 보장 |

---

## 8. 전수조사 추가분 — 미분석 커밋(~2,700개)에서 발굴한 신규 항목

**범위**: `epics-base` 3.15 이후 전체 4,083개 커밋 중 기존 triage 데이터(1,370개)에 없던 나머지 ~2,700개 커밋을 도메인별로 필터링하여 직접 분석.

---

### 8-A. CA/PVA 프로토콜 & 서버

- **`EPICS_IOC_IGNORE_SERVERS` 환경 변수** (`6efe2924`, 2017): 특정 CA 서버를 IOC 내부에서 완전히 무시하도록 필터링하는 환경 변수.
- **`EPICS_CA_MCAST_TTL` 환경 변수** (`f2a1834d`, 2017, 3.16): CA 멀티캐스트 패킷의 TTL(Time-To-Live)을 설정하는 환경 변수. `epics-ca-rs`의 UDP 소켓 멀티캐스트 TTL 설정 확인.
- **rsrv: 최대 배열 바이트(max array bytes)를 초과하는 큰 배열 지원** (`3009f88f`/`85b6b5c5`, 2017): CA/PCAS 서버와 클라이언트가 `EPICS_CA_MAX_ARRAY_BYTES` 한계보다 큰 배열을 처리할 수 있는 기능.
- **rsrv 멀티 인터페이스 바인딩 재구성** (`15307c4d`, 2016): 여러 NIC에 CA 서버를 각각 바인딩하는 초기화 로직 재설계.
- **camonitor 데이터 타입 변경 처리** (`16877577`, 2020, 3.15.7): 서버가 채널의 DBR 타입을 변경할 때 `camonitor`가 동적으로 처리하는 기능.
- **mcast loopback 소켓 옵션 활성화** (`98504d1c`, 2016): CA 멀티캐스트 루프백 소켓 옵션(`IP_MULTICAST_LOOP`) 명시적 활성화.
- **`casr()` 출력 개선** (`1c1eb030`, 2016): CA 서버 보고 명령의 출력 형식 개선.
- **`EPICS_NO_CALLBACK` 환경 변수** (`75a1b823`, 2019): 콜백 시스템을 런타임에 비활성화하는 옵션. `epics-rs`의 콜백 옵트아웃 로직 여부 확인.
- **`CASDEBUG` 환경 변수를 `iocsh`에 노출** (`546df1c1`, 2017): RSRV 디버그 레벨을 iocsh에서 직접 설정할 수 있는 기능.

---

### 8-B. 레코드 타입 / 필드

- **`subArray` 레코드 개선 및 소프트 디바이스 지원** (`d1af6637`, 2017): `subArray` 레코드에 소프트 채널 디바이스 지원 및 다양한 엔핸스먼트 추가. → 섹션 2의 배열 레코드 미구현 항목에 포함.
- **`int64in`/`int64out` 레코드의 모니터 델타 버그 수정** (`3091f7c5`, 2021): 64비트 정수 레코드의 변화량 기반 모니터 발송 로직 버그.
- **`PUTF`를 통해 `DB_LINK` 및 `RPRO` 비동기 전파** (`a4fcd229`, 2018): Put Flag(`PUTF`)가 데이터베이스 링크와 Record Process(`RPRO`) 경로를 통해 올바르게 전파되는 로직.
- **`dbCa` CP 링크 업데이트 시 `PUTF`/`RPRO` 설정** (`a4bc0db6`, 2024): CA 링크가 값을 업데이트할 때 레코드의 Put Flag와 Reprocess 플래그를 올바르게 설정해야 함.
- **`scanOnceCallback()` 완료 콜백 지원** (`2ba2b90b`/`bbbf0541`, 2015): `scanOnce`가 완료될 때 콜백을 받는 `scanOnceCallback()` API.
- **`dbScan`: I/O Intr 목록 직접 스캔 지원** (`7d50f62a`, 2015): I/O 인터럽트 스캔 리스트를 직접 순회하는 기능.
- **`dbCa`: 가변 길이 배열 구독** (`b2716f0a`, 2015): CA 링크에서 가변 길이 배열을 구독할 때 NORD 변화를 올바르게 처리하는 로직.
- **`aSub` 레코드 INAM 변경 시 출력 처리** (`2af98c33`, 2017): `INAM`(Init Name)을 변경했을 때 출력 링크를 재설정하는 로직.
- **`aSub` 레코드의 올바른 데이터 복사량** (`52787995`, 2017): 배열 데이터를 복사할 때 `BPTR` 오프셋 계산 버그(정확한 크기보다 더/덜 복사하는 버그).
- **`asTrapWrite`에 Put 데이터 제공** (`c5ded306`, 2015): Access Security Trap에서 실제 Put한 데이터를 함께 노출하는 확장.
- **`xRecord` 디바이스 지원** (`b9cbf7a3`, 2015): 모든 타입의 디바이스를 연결할 수 있는 범용 `xRecord`.

---

### 8-C. DB/링크 시스템

- **JSON Links 시스템 도입** (`7edc0c67`, 2016): 링크 타입을 JSON 형식으로 기술하는 새로운 링크 모델(`lnkCalc`, `lnkConst` 등). `epics-rs`의 DB 로더가 JSON 링크를 파싱하는지 확인 필요.
- **`lnkCalc` 링크 타입의 타임스탬프 지원** (`e3c9d590`/`20404003`, 2017/2018): Calc 링크(`lnkCalc`)가 타임스탬프를 처리하는 기능.
- **`dbLink`의 필드 타입을 `DOUBLE`로 반환** (`9813fa64`, 2015): 링크 필드가 숫자 값을 읽을 때 `DOUBLE`로 캐스팅하는 경로.
- **링크 필드의 긴 문자열 버퍼 크기 확장** (`19447dc7`, 2016): `INP`/`OUT` 링크 필드 버퍼를 128바이트 이상으로 확장하는 패치.
- **`dbPutStringNum("", ...)` 을 오류로 처리하지 않음** (`0821c8c4`, 2016): 빈 문자열로 숫자 필드에 Put 시 오류가 아닌 무시 처리.
- **`dbLinkDoLocked()` 지원** (`d2db634e`, 2017): 레코드가 잠긴 상태에서 링크 작업을 수행하는 API.
- **`iocshFindCommand()` API** (`9d7c4434`, 2017): 등록된 iocsh 명령을 이름으로 조회하는 API. `epics-rs`의 iocsh 명령 레지스트리 노출 여부 확인.
- **`dbRecordsAbcSorted`: 알파벳 순 레코드 목록** (`a32faa57`, 2016): 레코드를 알파벳 순으로 정렬하여 조회하는 iocsh 명령.
- **`dbStatic`: 알파벳 정렬 옵션(opt-in)** (`336bd656`, 2016): 레코드 정렬을 기본적으로 비활성화하고 명시적으로 켜는 옵션.
- **빈 배열(`""`) 입력 링크 허용** (`ec650e8c`, 2022): 빈 문자열을 입력 링크로 허용하는 파서 시맨틱.

---

### 8-D. iocsh / 런타임 / 환경

> 본 세션 일괄: **⏸️ DEFERRED** (단, `dbServerStats`는 🔄 PARTIAL `ac92e3e` — 섹션 2 참조).

- **iocsh 스크립트 include 시 echo 비활성화 옵션** (`0fd07d16`, 2016): `< script.cmd` 등으로 스크립트를 실행할 때 각 명령줄의 에코를 끄는 옵션.
- **`dbStopServers()` 를 `iocShutdown()`에 포함** (`a9393242`, 2017): IOC 셧다운 시 CA 서버를 명시적으로 정지하는 로직. `epics-rs`의 `iocShutdown` 경로 확인.
- **`readline`을 `epicsExit()`에서 정리** (`444b89f5`, 2015): `epicsExit` 시 readline 라이브러리를 정상적으로 해제하는 훅.
- **`EPICS_TZ` 환경 변수로 표준화** (`b0db6568`, 2019) — ⚠️ **N/A**: 원 commit은 RTEMS `rtems_init()`에서 `EPICS_TIMEZONE` 대신 `EPICS_TZ`를 읽도록 변경. Rust는 RTEMS 비대상 + `chrono::Local` 등이 OS POSIX `TZ` 환경변수를 자동 사용하므로 EPICS-namespaced timezone env var를 별도로 다룰 진입점이 없음.
- **`generalTime`의 이벤트 번호 >= 256 지원** (`215c5d95`, 2018): `NUM_TIME_EVENTS` 이상의 이벤트 코드를 타임스탬프 프로바이더에서 처리하는 기능. (→ RELEASE-3.16.md 항목과 동일)
- **`osiClockTime` 동기화 훅 지원** (`5cfff383`, 2019): 외부 시간 소스가 동기화될 때 알림을 받는 훅 인터페이스.
- **`epicsTime` UTC `struct tm` 전체 변환** (`37024011`, 2016): 타임존 없이 UTC 기반으로 `struct tm`과 완전한 상호 변환을 지원하는 API.
- **`envGetBoolConfigParam` 함수** (`f837add8`, 2016): 환경 변수를 bool 값으로 읽는 유틸리티 함수. `epics-rs`의 `runtime::env` 모듈 확인.
- **`iocsh`에 등록된 변수/함수 목록 조회 API** (`daad3c69`, 2016): `iocshFindVariable()` 등 등록된 iocsh 심볼을 열거하는 API.
- **`dbServerStats()` API** (`bcc6cb96`/`350570134`, 2025, PR #592): → 섹션 2 기존 항목과 동일.
- **iocsh ANSI 컬러 출력** (`c0da3dd`, 2025): iocsh 프롬프트 및 에러/경고 메시지에 ANSI 컬러 코드 적용. `epics-rs` iocsh의 UX 개선 참고.

---

### 8-E. 스캔 / 이벤트 / 콜백

> 본 세션 일괄: **⏸️ DEFERRED**.

- **`epicsCallback` 타입 도입** (`00a974ce`/`73fec881`, 2018/2019): `CALLBACK` 구조체의 타입-안전 래퍼 `epicsCallback` 추가.
- **콜백 큐 상태(callback queue status) 노출** (`59ec8d89`, 2018): 콜백 큐의 현재 상태(사용량, 오버플로 횟수 등)를 조회하는 API. `epics-rs`의 콜백 큐 모니터링 노출 확인.
- **`EPICS_NO_CALLBACK` 옵션** (`75a1b823`, 2019): 콜백 처리 시스템 전체를 런타임에 비활성화. → 8-A와 동일.
- **dbScanPassive를 `dbDbLink.c`로 이동** (`7626856a`, 2018): EPICS 링크 아키텍처 개편에 따른 내부 구조 변경.
- **주기 스캔 속도 보호** (`49e0e23f`, 2017): 너무 빠른 주기 스캔 속도가 입력될 때 보호 로직(최소값 클램프).
- **dbCa: `dbCaPutLinkCallback`의 초기화 버그** (`c0cf25ee`/`3501fda4`, 2015): CA 링크 Put 콜백 시 전체 배열을 초기화하지 않거나 배열 경계를 넘어 쓰는 버그.

---

### 8-F. 필터 시스템

> 본 세션 일괄: **⏸️ DEFERRED** — Section 1의 "서버 측 채널 필터" 항목 종속. 필터 프레임워크 자체가 deferred이므로 그 안의 모든 필터별 버그도 자동으로 deferred.

- **`arr` 필터의 wrap이 `capacity` 기준으로 동작** (`840da801`, 2016): 배열 필터에서 wrapping 계산이 `length`가 아닌 `capacity` 기준으로 수행되어야 하는 버그 수정.
- **`sync` / `unless` 모드 필터의 메모리 누수** (`8ff6ce48`, 2019): sync 필터의 특정 모드에서 field-log가 누수되는 버그.
- **`decimate` 필터의 드롭된 field-log 누수** (`f79c69f0`, 2019): decimate 필터에서 드롭된 필드 로그를 해제하지 않는 버그. (→ `epics-rs` 서버 필터 구현 시 반드시 고려)

---

## 9. Archaeology Index 전수 감사 — 크레이트별 미반영 항목

**출처**: `archaeology/INDEX/master_index.md` (총 367개, applies 127건 + partial 44건)  
아래는 기존 섹션 1~8에서 다루지 않은 **`applies`/`partial` 판정 항목들**을 크레이트별 대응 파일과 함께 정리합니다.

---

### 9-A. `base-rs` — 수명주기 & 초기화 (Lifecycle)

> 본 세션 일괄: **⏸️ DEFERRED**. 9-A는 high-priority 라이프사이클 버그 다수 포함 — 별도 PR로 한 항목씩 정밀 작업 권장. `iocInit 로컬 CA 링크 대기`, `dbDbLink 자기-링크 RPRO 무한 루프`, `errlog 이중 버퍼링`, `PINI 힙 UAF` 등은 실용 영향 큰 우선 후보.

- **`waveform` 레코드 `PACT=TRUE` 유실** (`16c3202`, high) — ⚠️ **N/A (design diff)**: C 버그는 sync-completion 경로에서 `prec->pact = TRUE`가 누락되어 `recGblGetTimeStampSimm` + `monitor()` + `recGblFwdLink()` 호출 중 PACT=false로 관측되어 callback에서 record를 재진입할 위험. Rust 측 `processing.rs::process_record_with_links_inner`는 `RwLock<RecordInstance>` 쓰기 잠금이 sync 처리 전구간을 보호하고, `visited: HashSet<String>` 가 cascade re-entry를 차단. `processing` AtomicBool은 async write_begin 반환 시 set (L727), `complete_async_record_inner`에서 clear (L1200) — async-pending 상태만 표상. sync 경로에서는 RwLock + visited로 serialization이 강제되므로 PACT=TRUE 윈도우가 필요 없음. CA `.PACT` 모니터는 async write 동안만 1을 관측 (C와 다른 시맨틱이지만 안전 등가).
- **`errlog` 이중 버퍼링 재작성** (`29fa062`, high) — ⚠️ **N/A (eliminated)**: epics-rs는 C `errlog.c` (mutex-protected single buffer) 대신 `tracing` crate 사용. tracing의 subscriber 모델은 non-blocking writer + per-thread buffer가 표준이므로 "출력 중 lock 보유" 구조 자체가 부재. 이중 버퍼링이 해결하려던 lock contention 문제가 발생하지 않음.
- **`scanStop()` 전 `scanStart()` 누락 시 크래시/행(Hang)** (`0a6b9e4`, high) — ⚠️ **N/A (eliminated)**: epics-rs `server/scan.rs` 는 `start()`/`stop()` 같은 명시적 lifecycle API 없이 `run()` / `run_with_hooks()` 메소드로 tokio task로 spawn되며 종료는 task abort/drop으로 처리. "scanStart 전 scanStop 호출"이라는 시나리오 자체가 불가 — 미시작 상태에서는 spawn된 task 자체가 없어 abort 대상이 부재.
- **`db_field_log`: `dbfl_type_rec` 제거, `dbfl_type_ref` 통합** (`27fe3e4`, high) — ⚠️ **N/A (design diff)**: epics-rs 필터 framework는 `db_field_log` 구조체 대신 `MonitorEvent { snapshot: Snapshot, ... }` (record_instance.rs::notify_*)을 사용. C의 `dbfl_type_{rec,ref,val,buf}` 분기 자체가 부재 — `Snapshot` 이 필요한 모든 데이터(value+alarm+timestamp+display+control+enums)를 직접 보유. 통합 대상 분기가 없음.
- **`dbGet`: `db_field_log` vs 라이브 레코드 선택 조건 버그** (`56f05d7`, high) — ⚠️ **N/A (design diff)**: C `dbGet`은 `dbfl_has_copy()` 매크로로 field log의 cached copy를 가질지 live record fetch할지 분기. Rust `field_io::get_record_field_from_ca` / `db_access` 경로는 항상 record의 live 값을 `get_field`로 읽거나 monitor 이벤트의 snapshot을 사용 — cached vs live 분기 자체가 부재.
- **`dbDbLink processTarget` 자기-링크 RPRO 무한 루프** (`62c11c2`, high) — ⏭️ **ALREADY** (cycle 7, commit `f80f15a`): `processing.rs::process_record_with_links_inner` 의 `visited: HashSet<String>` 가드가 self-recursion을 차단. `dispatch_cp_targets`와 RPRO recheck 분기 모두 동일 가드 사용. 회귀 테스트 `test_self_link_out_does_not_loop` 가 1초 timeout으로 무한 재귀 regression을 fail-fast.
- **`dbPutFieldLink`: `dbChannelOpen()` 오류 상태 전파** (`8a0fc03`, high) — ⚠️ **N/A (eliminated)**: C 버그는 `if (chan && dbChannelOpen(chan) != 0) { goto cleanup; }` 가 status 변수에 에러 코드를 캡처하지 않아 cleanup이 stale/uninitialized status를 반환. Rust `Result<T, E>` + `?` 연산자가 에러 캡처를 강제 — `let chan = open_channel(...)?;` 패턴은 컴파일 타임에 보장. uninitialized status로 인한 잘못된 success 반환 패턴이 컴파일 차원에서 방지됨.
- **`db_field_log`: 데이터 소유권 추상화 누락** (`85822f3`, high) — ⚠️ **N/A (eliminated)**: C 버그는 `db_field_log` 가 데이터 소유권을 추적하지 않아 scan lock 외부에서 record 데이터에 접근. Rust 측 `MonitorEvent::snapshot: Snapshot` 가 `EpicsValue::*Array(Vec<...>)` 등 owning value를 직접 보유 — record lock 외부 접근 시점에는 이미 owned copy가 만들어진 상태. ownership semantics가 컴파일 타임에 데이터 race를 차단.
- **`callbackRequest`: 미초기화 콜백 큐 접근** (`ac6eb5e`, high) — ⚠️ **N/A (design diff)**: C `callback.c` 큐 시스템 자체를 사용하지 않음. epics-rs는 tokio 런타임의 `mpsc::channel` + spawn으로 콜백 워크 처리 — `callbackInit()` 등가 초기화 단계 부재. 채널은 생성 시점에 즉시 사용 가능 (tokio runtime이 보장). "미초기화 큐에 push" 시나리오 자체가 불가.
- **`PINI` 크래시: 힙 Use-after-free 방지를 위한 스택 필드-로그** (`e0dfb6c`, high) — ⚠️ **N/A (eliminated)**: C 버그는 PINI 처리 중 filter chain에 전달된 heap-allocated `db_field_log`가 chain 종료 후 free되어 후속 monitor에서 UAF. Rust 측 `MonitorEvent::snapshot: Snapshot` 은 `Clone` 으로 매 subscriber 전달 시 owned copy 생성 (`record_instance.rs:1651` `make_monitor_snapshot(value.clone())`). UAF 자체가 ownership 시스템에서 컴파일 타임 차단.
- **`dbEvent` 안전한 종료 세마포어 셧다운 프로토콜** (`b35064d`, high) — ⚠️ **N/A (design diff)**: C `dbEvent` 워커 스레드를 join하던 시도가 데드락을 유발하는 패턴 — 세마포어 기반 셧다운 프로토콜로 revert. epics-rs는 dbEvent 워커 스레드가 없음 — `record_instance::notify_*` 가 호출자 task 컨텍스트에서 `mpsc::Sender::try_send` 로 직접 fan-out. 별도 워커가 없으므로 join/shutdown 프로토콜 자체가 부재. tokio task termination은 abort/drop으로 처리.
- **`dbEvent` 다중 `db_event_cancel()` 호출 안전성** (`fab8fd7`, high) — ⚠️ **N/A (eliminated)**: C `db_event_cancel()`을 여러 번 호출하면 free된 구조체에 접근. epics-rs는 subscription을 `Vec<Subscriber>` 의 entry로 보유하고 `RecordInstance::remove_subscriber(sid)` 가 `subscribers.values_mut().for_each(|v| v.retain(|s| s.sid != sid))` 로 멱등 — 동일 sid를 여러 번 remove 호출해도 두 번째부터는 retain이 no-op. 별도 가드 없이도 multi-call safe.
- **`asCaStop()` 스레드 join 데드락 방지** (`bac8851`, high) — ⚠️ **N/A (design diff)**: C `asCaStop()` worker thread join이 lock 순서 cycle로 데드락 — revert로 join 제거. epics-rs ACF runtime은 별도 worker thread 없이 `epics_base_rs::server::access_control` 의 in-process 평가만 사용 — join 대상 thread 자체가 부재. ACF reload는 `Arc<RwLock<AccessConfig>>` swap으로 atomic, lock 순서 cycle 가능성 없음.
- **`iocInit` 로컬 CA 링크 연결 대기** (`717d69e`, high): `PINI` 처리 전에 로컬 CA 링크가 연결될 때까지 대기. → `ca_link.rs`
- **`longout` `OOPT=On Change` 첫 처리 시 출력 누락** (`6c573b4`, medium) — ⏭️ **ALREADY** (cycle 6, commit `4e4fd49`): `LongoutRecord::first_output_done` flag 가 첫 cycle을 항상 emit. `compute_should_output()` 가 first_output_done==false 시 early-return true, `on_output_complete()`가 flag set. 회귀 테스트 `test_longout_oopt_on_change_first_cycle_emits_then_suppresses`.
- **`longout special()`: 링크 변경 플래그를 OUT 링크 갱신 전에 설정** (`1d85bc7`, medium) — ⚠️ **N/A (design diff)**: C `special(SPC_LINK_CHANGE)` 가 OUT 필드 put `before` 단계에서 호출되어 outpvt=CHANGED 플래그를 설정한 뒤 OUT 문자열이 갱신되는 순서 문제. epics-rs는 `record_instance.rs::put_common_field("OUT", ...)` 가 단일 critical section 안에서 (1) `common.out = s` 저장 (2) `parsed_out = parse_link_v2(&self.common.out)` 재파싱 — 두 동작이 atomic. 별도 SPC_LINK_CHANGE 콜백 없이 framework가 다음 process cycle에 새 parsed_out 사용. 순서 race 자체가 부재.
- **`mbboDirect`: 초기화 우선순위 버그 — `B0..B1F` bits가 `VAL`보다 우선** (`dabcf89`, medium) — ✅ **DONE**: 신규 trait 메소드 `Record::post_init_finalize_undef(&mut bool)` 추가, `ioc_builder` 가 `init_record` 양 pass 후 호출. `MbboDirectRecord` override가 (1) UDF=false → `val_to_bits()` (2) UDF=true && bits any-set → `bits_to_val()` + UDF clear (3) 둘 다 아니면 no-op. 회귀 테스트 `test_mbbo_direct_initialises_val_from_bits_when_undef` (3 케이스: undef+bits→VAL / VAL→bits / undef+nothing→유지).
- **`aai`/`waveform` 레코드 `NORD` db_post_events 정리** (`23d9176`/`5d1f572`/`aff7463`, medium) — ⏭️ **ALREADY** (cycle 1+3, commits `0b4e89a`, `1400bd8`): NORD 이벤트는 record 레이어(`record_instance.rs::notify_*` snapshot path) 단일 진입점만 사용 — device support는 직접 db_post_events 호출 없음. set_val 사이드이펙트도 put_pv_and_post에서 명시적으로 NORD subscribe 알림 (cycle 3).
- **`subArray` `NORD` 변화 시 `db_post_events` 누락** (`51c5b8f`/`64011ba`, medium) — ⏭️ **ALREADY** (cycle 1, commit `0b4e89a`): `WaveformRecord::set_val` SubArray 분기가 NORD를 항상 갱신, snapshot path 가 NORD subscriber에게 post-process timestamp로 전달. 4 ArrayKind 모두 회귀 테스트 `test_array_records_nord_monitor_uses_post_process_timestamp`.
- **`AMSG` 알람 메시지가 MSS 링크를 통해 전파되지 않음** (`d0cf47c`, medium) — ✅ **DONE**: 멀티-입력 경로 (INPA..INPL — calc/sub/aSub/sel)는 이미 `rec_gbl_set_sevr_msg`로 alarm.stat/sevr/amsg 전파를 처리했으나, 단일 INP 경로 (ai/longin/bi/mbbi/stringin)에서 누락됨. `processing.rs::process_record_with_links_inner`이 `is_soft && inp_parsed = Db(_)`일 때 `read_link_with_alarm`로 소스의 STAT/SEVR/AMSG를 읽고 `link_alarms` 리스트에 push해서 기존 MS 처리 루프가 동일하게 maximize 처리하도록 통합. 회귀 테스트: `test_single_inp_ms_class_propagates_source_alarm` (ai DST `INP="SRC NPP MS"`, SRC sevr=Major/amsg="src-major" → DST가 Major/HIHI/"src-major"로 lift).
- **타임스탬프가 출력 링크 처리 후 갱신되어 `TSEL` 스탤 타임스탬프 발생** (`f1e83b2`, medium) — ⏭️ **ALREADY** (cycle 5, commit `06aa884`): `process_record_with_links_inner` 가 apply_timestamp(L623) → OUT 스테이지(L668-764) → snapshot/notify(L787-866) → write_db_link_value(L870) 순으로 구조적 보장. complete_async_record_inner도 apply_timestamp(L1192) 먼저. 회귀 테스트 2종 (cascade + async completion).
- **`dbNotify`: 첫 번째 레코드 호출에서만 `PUTF` 설정** (`3fb10b6`, medium) — ✅ **DONE**: `processing.rs::dispatch_cp_targets`가 CP target에 `tg.common.putf=true`를 spuriously 설정하던 라인 제거. PUTF는 직접 dbPut을 받은 레코드만 `field_io.rs` put 경로에서 set/clear (line 390/463), CP/CPP 입력 트리거로 처리되는 다운스트림 레코드는 PUTF=false 유지. 회귀 테스트: `test_putf_stays_off_for_cp_chained_targets` (SRC→TGT CP, SRC process 후 TGT.putf=false 확인).
- **`devAiSoft read_ai`: 디바이스 읽기 실패 시 오류 반환** (`4737901`, medium) — ✅ **DONE**: `processing.rs::process_record_with_links_inner`에서 soft-channel DTYP의 INP read 가 `None`을 반환하고 (`read_link_value_soft → get_pv → Err`) 그 INP가 실제 Db/Ca/Pva 링크였을 때 `rec_gbl_set_sevr(LINK_ALARM, INVALID)`로 알람 전파. 기존엔 silently `None` → 정상 처리 종료로 broken link 가 invisible. 회귀 테스트: `test_soft_inp_read_failure_sets_link_alarm` (database_tests.rs).
- **`initHookRegister` 멱등성 보장** (`13d6ca5`, medium) — ⚠️ **N/A (design diff)**: epics-rs `IocApplication::after_init_hooks: Vec<Box<dyn FnOnce() + Send>>` 는 hook을 push만 함 — 같은 클로저를 두 번 push해도 두 인스턴스로 저장되며 두 번 실행됨. C 등가의 "함수 포인터 dedup" 패턴이 가능하려면 `dyn FnOnce`를 Eq로 만들 수 없으므로 사용자가 그 책임을 짐. 의도적 트레이드오프 — Rust 클로저는 unique 타입이라 dedup 시맨틱이 어색하고, 우리는 `IocApplication::run`이 한 번만 호출되는 패턴을 강제.
- **`iocShutdown`에서 de-init hook 알림 추가** (`5d5e552`, partial) — ⚠️ **N/A (eliminated)**: epics-rs는 명시적 `iocShutdown` 함수 없이 `IocApplication::run`이 select!로 SIGINT/SIGTERM을 받으면 모든 spawn된 task가 RAII로 정리. tokio task drop이 graceful shutdown 등가. de-init hook 등가 기능이 필요해질 때 `IocApplication::shutdown_hooks: Vec<Box<dyn FnOnce()>>` 추가 가능 — 현재 사용 사례 없음.
- **`errlog` 워커가 셧다운 전 버퍼를 비우지 않고 루프 종료** (`7448a8b`, partial) — ⚠️ **N/A (eliminated)**: 29fa062와 동일 — `tracing` crate 사용. tracing subscriber는 종료 시 `Drop`이 buffer flush를 보장하며 (예: `tracing-appender::non_blocking`의 WorkerGuard) 별도 drain 시퀀스 불필요.
- **`errSymbolAdd`가 `errSymBld` 전에 실패** (`8c08c57`, medium) — ⚠️ **N/A (eliminated)**: errSymbol 테이블 자체가 부재 — Rust는 `CaError` enum + `thiserror::Error` derive 로 컴파일 타임에 모든 에러 변형이 결정됨. 런타임 심볼 등록/조회 단계가 없으므로 초기화 순서 race 자체가 불가.
- **`ts` 필터: 오래된 `db_field_log` API 사용** (`e11f880`, partial) — ⚠️ **N/A (eliminated)**: epics-rs `ts` 필터(`crates/epics-base-rs/src/server/database/filters/ts.rs`)는 `MonitorEvent::snapshot.timestamp`를 직접 mutate — `db_field_log` 구조 자체를 사용하지 않으므로 union 변경에 영향받지 않음.
- **`Decimate`/`Sync` 필터가 `DBE_PROPERTY` 이벤트를 잘못 드롭** (`a74789d`, high): → 섹션 7-F의 기존 항목 보강 (master_index에 `base-rs/src/server/database/filters/decimate.rs`로 명시).
- **`dbEvent` 이벤트 큐 중복 참조 타입 이벤트 누적** (`4df48c9`, medium) — ⏭️ **ALREADY** (different mechanism): epics-rs 모니터 fan-out은 `Subscriber::tx: mpsc::Sender<MonitorEvent>` (cap 64) + `coalesced: Arc<Mutex<Option<MonitorEvent>>>` 패턴 (`record_instance.rs:1772-1778`). channel full 시 `try_send` 실패하면 `coalesced` slot에 last-value-only로 보관 — 중복 누적 자체가 구조적으로 불가. C 의 "compaction" 의도와 동등 결과.
- **`compressRecord`: `RES` 필드로 리셋 시 모니터 이벤트 미발송** (`8ac2c87`, medium) — ⏭️ **ALREADY**: `put_record_field_from_ca`가 RES put 후 process를 호출하고 `compress.process()` (records/compress.rs:194-201)가 `res != 0` 감지 시 reset 수행, 이어서 snapshot include_val 경로가 빈 VAL을 monitor 이벤트로 푸시. `put_field("RES")` 자체에도 즉시 reset 로직 추가 (defensive for non-processing direct-put 경로). 회귀 테스트 `test_compress_res_write_posts_val_monitor` (`tests/database_tests.rs`).

---

### 9-B. `base-rs` — 경계 및 타입 (Bounds / Type-system)

> 본 세션 일괄: **⏸️ DEFERRED**.

- **`histogramRecord` wdog 콜백이 `VAL` 대신 `bptr`로 이벤트 발송** (`4a0f488`, medium): 히스토그램 레코드의 잘못된 포인터로 모니터 이벤트 발송.
- **영(zero)원소 배열 읽기에 대한 고유 오류 코드** (`5d808b7`+`3b3261c`, medium) — ⏭️ **ALREADY (post-revert)**: 섹션 5 보강 참조. upstream이 `S_db_emptyArray`를 revert하여 다시 `S_db_badField` 사용, Rust `CaError::InvalidValue`가 동일 의미.
- **`.DTYP` 없는 레코드 타입에서 `DTYP` 조회 시 크래시 대신 빈 문자열** (`6e7a715`, medium): 디바이스 지원이 없는 레코드에서 `.DTYP` 조회 시 안전하게 처리.
- **`get_enum_strs` 포인터 산술이 `_FORTIFY_SOURCE=3`에서 경고** (`979dde8`, medium): 열거형 문자열 배열 접근 방식이 강화된 컴파일러 보안 검사에서 걸리는 패턴.
- **`lsi`/`lso` `SIZV` 필드가 32767에서 오버플로** (`e5b4829`, medium → 7-B 항목 보강): `dbAddr::field_size`가 부호 있는 정수여서 32768 이상에서 오버플로.
- **`compressRecord` `compress_scalar` 평균 계산 버그** (`11a4bed`, partial → 7-B 항목 보강).
- **`compressRecord` `compress_array`: `PBUF=YES`일 때 부분 버퍼 거부** (`84f4771`, partial): 부분 채워진 버퍼로 압축 시 유효한 데이터를 잘못 거부하는 버그.
- **`dbPutConvertJSON`: 빈 JSON 문자열이 yajl에 전달되어 파싱 오류** (`ec650e8`, partial): 빈 문자열 입력에 대한 사전 검사 누락.
- **`epicsNAN`/`epicsINF`를 모든 플랫폼에서 진정한 const로** (`5485ada`, medium): 컴파일 타임 상수로 선언되었으나 일부 플랫폼에서 런타임에 초기화되는 문제.
- **`DBF_CHAR` waveform 필드에 대한 상수 링크 문자열 초기화 실패** (`b36e526`, medium): `DBF_CHAR` 타입 배열 필드를 상수 링크로 초기화할 때 실패하는 엣지 케이스.
- **`struct link::flags` 부호 있는 비트 필드 UB** (`e88a186`, medium): 비트 필드에 부호 없는 타입을 사용해야 UB를 방지. Rust에서는 구조적으로 해결됨(확인 필요).
- **메뉴 필드 변환: 범위 초과 enum 인덱스에 대해 숫자 문자열 반환** (`b460c26`, partial): 유효 범위를 벗어난 enum 값을 숫자 문자열로 폴백해야 함.

---

### 9-C. `base-rs` — 흐름 제어 (Flow-Control)

> 본 세션 일괄: **⏸️ DEFERRED**.

- **`logClient`: 연결 끊김 시 미전송 버퍼 버리지 않기** (`0a3427c`, medium) — ⚠️ **N/A (design diff)** — 섹션 7-C 항목과 동일 (logClient TCP forwarder 미구현).
- **필터가 DB 링크 읽기 경로(`dbDbGetValue`)에 적용되지 않음** (`17a8dbc`, medium): DB 링크로 값을 읽을 때 서버 필터가 바이패스되는 구조적 누락. → `db_db_link.rs`
- **DB 링크가 `dbChannel` 대신 `DBADDR`를 저장하여 필터 메타데이터 손실** (`b1f4459`, medium): → 섹션 7-C의 `dbChannel` 교체 항목 보강.
- **`logClient` 재연결 후 미전송 메시지 즉시 플러시되지 않음** (`9df98c1`, partial): 재연결 후 버퍼에 쌓인 로그가 즉시 전송되지 않는 문제.

---

### 9-D. `ca-rs` — 네트워크 라우팅 (Network-Routing)

- **`rsrv`: 클라이언트 공급 호스트명 대신 검증된 IP 주소 사용** (`530eba1`, high) — ⏭️ **ALREADY**: `EPICS_CAS_USE_HOST_NAMES=NO` 기본값으로 peer IP를 권위로 사용; `=YES`일 때 `host_resolves_to_peer`로 forward-DNS 검증.
- **`RSRV_SERVER_PORT` 9999 초과 포트 번호에서 잘림** (`772c10d`, high) — ⚠️ **N/A**: Rust의 `u16` 사용으로 0..=65535 전 범위 안전.
- **SO_REUSEPORT + SO_REUSEADDR 함께 설정 (Linux)** (`5064931`, medium) — ⏭️ **ALREADY**: `AsyncUdpV4::bind_one_at`이 양 옵션 설정 (`set_reuse_address` + `set_reuse_port`).
- **BSD에서 `SO_REUSEADDR`만으로는 부족 — `SO_REUSEPORT` 필요** (`65ef6e9`, medium) — ⏭️ **ALREADY**: 위와 동일.
- **RSRV 반복 비콘 UDP 전송 오류 메시지 억제** (`c23012d`, medium) — ⏭️ **ALREADY**: `server/beacon.rs::run_beacon_emitter`에 per-destination first/change/recovery dedup.
- **CA 클라이언트 UDP 전송 오류 억제(목적지별)** (`cae597d`, medium) — ⏭️ **ALREADY**: `client/search.rs::send_with_fanout`에 동일 패턴.
- **asLib: DNS 조회 실패 시 소프트 폴백** (`932e9f3`, partial) — ✅ **DONE** `6862ef0`.
- **네트워크 인터페이스 열거에서 `SIOCGIFCONF` → `getifaddrs` 교체** (`410921b`, partial) — ⏭️ **ALREADY**: `get_if_addrs` crate 사용 (cross-platform).
- **`caRepeater` 부모 프로세스의 `stdin`/`stdout`/`stderr` 상속 문제** (`6dba2ec`, partial) — ✅ **DONE**: `ca-repeater-rs`가 Unix에서 기본적으로 `/dev/null`로 `dup2` (`crates/epics-ca-rs/src/bin/ca-repeater-rs.rs`). C `caRepeater.cpp`의 `CAN_DETACH_STDINOUT` 가드와 동일하게 Linux/macOS에서만 detach, Windows/RTEMS/VxWorks 패스. `-v/--verbose` flag로 기존 stdio 유지 (디버깅용). clap 기반 arg parsing.

---

### 9-E. `base-rs` — 와이어 프로토콜 (Wire-Protocol)

> 본 세션 일괄: **⏸️ DEFERRED**.

- **`dbPut` long-string(nRequest>1) 경로에서 `get_array_info` 스킵** (`82ec539`, medium): 긴 문자열 Put 시 배열 정보를 가져오지 않아 쓰기 경로가 손상. → `db_access.rs`
- **`db_field_log` DBE 마스크 누락으로 필터가 `DBE_PROPERTY` 구분 불가** (`235f8ed`, medium): → 섹션 7-A의 기존 항목 보강.
- **`caput`으로 0원소 배열 쓰기 허용** (`a42197f`, medium): 빈 배열 전송을 지원. → 섹션 7-A 항목 보강.
- **CA count=0이 가변 크기 배열 구독을 의미함을 문서화** (`8c99340`, low): count=0의 시맨틱 명확화.

---

### 9-F. `base-rs` / `ca-rs` — 기타 (Other)

> 본 세션 일괄: **⏸️ DEFERRED**.

- **`recGblRecordError`: 음수 상태 코드에 대한 오류 심볼 조회 건너뜀** (`4c20518`, medium): 음수 오류 코드를 받았을 때 심볼 이름 조회를 건너뛰어 오류 메시지가 불명확.
- **`iocsh` 인자 분리기: EOF 센티널 (-1)이 유효 문자로 처리** (`3dbc9ea`, partial): `iocsh` 파서에서 -1이 EOF가 아닌 정수로 처리되는 버그. → `iocsh/mod.rs`
- **`aSub` 레코드: 상수 입력 링크에 `dbGetLink` 호출 오류** (`d47fa4c`, partial) — ⏭️ **ALREADY**: 섹션 5/7-C 보강 참조 (multi-input fetch가 `ParsedLink::Constant`를 별도 분기로 처리).
- **`subRecord`: 잘못된 `INP` 링크 오류를 조용히 성공으로 처리** (`832abbd`, partial): 불량 입력 링크의 오류를 무시하는 버그.
- **`iocsh`에 `iocshSetError`로 오류 코드 전파** (`144f975`, partial): → 섹션 7-C 기존 항목 보강.
- **`waveform` `NORD`가 타임스탬프 갱신 전에 발송 → 첫 CA 모니터에 미정의 타임스탬프** (`5ba8080`, medium): `NORD` 이벤트와 타임스탬프 갱신 순서 문제. → `waveform` 레코드

---

### 9-G. new-notes PR (최신 미병합 기능)

`documentation/new-notes/`에서 발굴한 현재 개발 중이거나 병합 예정인 기능들:

- **PR #359: `aai`/`aao`/`subArray`/`waveform`의 `NORD` 필드 타임스탬프 버그 수정** — ✅ **DONE**: 레코드 타입 `a02c310`로 구현 + NORD 타임스탬프 순서 수정은 구조적으로 enforced. Rust 측 모든 notify 경로(`process_record_with_links_inner`, `AsyncPendingNotify`, `complete_async_record_inner`)가 `apply_timestamp` → snapshot 빌드 → `notify_from_snapshot` 순서로 호출되므로 NORD 이벤트는 항상 post-process timestamp를 carries. 회귀 테스트 `test_array_records_nord_monitor_uses_post_process_timestamp` (`crates/epics-base-rs/tests/database_tests.rs`)이 4 종류(Waveform/Aai/Aao/SubArray) 모두 검증.
- **PR #768: `iocInit`에서 로컬 CA 링크 연결 대기** — ✅ **DONE**: `IocApplication::run`이 `setup_cp_links` 직후 `PvDatabase::wait_for_external_links(timeout)`를 호출해 모든 등록된 LinkSet(dbCa/dbPv 양쪽)의 외부 링크가 connect될 때까지 대기. timeout은 `EPICS_RS_INIT_LINK_TIMEOUT` env (기본 10초, 0이면 wait skip). C의 `initOutstanding`/`DBCA_CALLBACK_INIT_WAIT` 카운터 + 별도 hook을 LinkSet trait의 `is_connected(name)` 폴링으로 단순화 — 결과 동등. `wait_for_external_links_*` 단위 테스트 3종(no lsets / quick-connect / partial-on-timeout).
- **PR #788: `epicsThreadGetCPUs` 및 `callbackParallelThreads` CPU 어피니티 반영** — ⏭️ **ALREADY**: 섹션 3 보강 (Rust `std::thread::available_parallelism()`).
- **PR #812: `dbCreateRecord` iocsh 명령어** — ⏭️ **ALREADY**: `cmd_db_create_record` (`crates/epics-base-rs/src/server/iocsh/commands.rs:680`)로 등록 + 5종 테스트(happy path + duplicate / bad name / unknown type / missing args).
- **PR #817: `mbbi` 레코드의 `AFTC`/`LALM` 버그 수정** — ✅ **DONE**: 세 항목 모두 이미 구현된 상태로 회귀 테스트 추가. (1) bi/mbbi AFTC 저역통과 필터는 `RecordInstance::aftc_filter` (record_instance.rs:1066) static helper로 중앙화돼 `evaluate_alarms`의 bi (L1182-1200) / mbbi (L1280-1300) 분기에서 호출됨. (2) AFVL writeback은 `record.put_field("AFVL", …)`로 매 cycle 기록. (3) mbbi COSV/LALM 갱신은 `if val != lalm { … set_sevr if cosv≠NoAlarm; put_field("LALM", val) }` 구조로 처음부터 post-fix shape — LALM은 cosv 설정 여부와 무관하게 val 변경 시 항상 update. 회귀 테스트 4종 (`test_bi_aftc_seeds_afvl_on_initial_sample`, `test_mbbi_aftc_writes_afvl_back_each_cycle`, `test_bi_lalm_updates_when_cosv_set`, `test_mbbi_lalm_updates_when_cosv_set`) 추가, 기존 pure-function `aftc_filter_tests` (5종) 보완.

---

## 10. Archaeology PVXS 감사 — `pva-rs` 미반영 고위험 항목

**출처**: `archaeology/pvxs/INDEX/master_index.md` (PVA/PVXS 구현체의 전체 커밋 대상 분석 결과)  
아래는 기존 PVA(pvAccess) 프로토콜 관련 미반영 항목들 중 `applies`(반영 필요) 판정을 받은 **High / Medium** 항목들입니다.

---

### 10-A. `pva-rs` — 클라이언트 & 네트워크 연결 (Client & Connection)

- **TCP Search 기능 추가** (`8363c7fe9a5f`, high) — ⏭️ **ALREADY**: `EPICS_PVA_NAME_SERVERS` 환경변수로 TCP name server 지원 (`client_native/channel.rs::new_with_name_servers`).
- **재연결 루프 지연(Slow down reconnect loop)** (`3b8540f52002`, high) — ⏭️ **ALREADY**: `channel.rs::holdoff_until` 타이머로 connect-fail holdoff 구현.
- **종료(Shutdown) 중 Name Server 재연결 금지** (`4d12da87205e`, high) — ✅ **DONE**: `ConnectionPool`에 `shutdown: AtomicBool` 추가, `clear()`(PvaClient::close 경로)가 release-store로 set. `Channel::ensure_active`의 name-server fallback이 `pool.is_shutdown()`을 체크해 true일 때 후보 목록에 추가하지 않음 — 이 경로가 pvxs 4d12da87이 `context->state==Running` 분기로 막던 것과 동일 효과. 테스트 2종(clear→true 전이, fresh→false 유지).
- **`Channel` Search Bypass 최적화** (`5d3a21f03010`, high) — ⏭️ **ALREADY**: `Channel::new_direct` (`crates/epics-pva-rs/src/client_native/channel.rs:315`)와 `PvaClient::channel`의 `forced.or(server_addr)` 분기 (`context.rs:298-311`)가 UDP search를 건너뛰고 지정된 server addr로 직접 TCP 연결. 코멘트에 "pvxs ConnectBuilder::server"로 출처 명시.
- **`Channel` 일관된 연결 해제(Disconnect) 처리** (`f7b3821e10b4`, high) — ⏭️ **ALREADY**: epics-pva-rs는 `Channel::set_state` (`channel.rs:678`)를 single entry point로 두고, 상태 전이 시 SID-close hook 등록/해제 + `server_destroyed` flag 처리를 한 곳에서 일관 처리. `close()`는 `set_state(Closed)`로 라우팅, 서버측 `CMD_DESTROY_CHANNEL` 수신 시에도 동일 경로(pvxs e668038 참조 코멘트). Rust Drop chain이 connector / op 정리를 보강.
- **`Context::close()` 명시적 지원** (`0de17036f4a6`, medium) — ⏭️ **ALREADY**: `PvaClient::close()` (`context.rs:610`).
- **Search 패킷 단편화(Fragmentation) 방지** (`84ef355a4a1a`, medium) — ⚠️ **N/A**: 현재 `build_search`가 count=1 단일 PV 패킷이라 MTU 미만 — batching 미구현이므로 fragmentation 발생 자체 불가.
- **환경 변수를 통한 설정 가능 타임아웃** (`da004bc54bb3`, medium) — ⏭️ **ALREADY**: `EPICS_PVA_CONN_TMO` 처리 `crates/epics-pva-rs/src/config/env.rs:243-248` + `Config`의 `tcp_timeout: Duration` 필드 (`client_native/context.rs:59-95`). pvxs convention의 4/3 scaling은 향후 적용 시점 검토.
- **Search 응답 처리 한계 상향** (`b38b33db034e`, medium) — ⏭️ **ALREADY (different mechanism)**: pvxs는 한 reactor iteration에 처리할 search 패킷 수를 4→40으로 올림. epics-pva-rs는 tokio `select!` 루프(`client_native/search_engine.rs:832`)가 `recv_from` 가용 시마다 즉시 처리하며 datagram 내부 멀티-메시지 드레인(P-G10)도 포함. 별도 per-iteration cap이 없어 backlog 누적 가능성이 구조적으로 적음.
- **Search 대상 목적지 없음 오류 로깅** (`8db40be29c81`, medium) — ⏭️ **ALREADY**: `search_engine.rs:202`에서 ADDR_LIST 비어있고 AUTO_ADDR_LIST=NO일 때 명시적 warning.

### 10-B. `pva-rs` — 서버 동작 & 세션 제어 (Server & Session)

> 본 세션 일괄: **⏸️ DEFERRED**.

- **공유 PV(SharedPV) 에러 경로 데드락 방지** (`b17f8207676d`, high): 에러 발생 시 내부 락(Lock) 해제 순서가 꼬여 데드락이 발생하는 문제 해결.
- **잘못된 SID(Session ID) 처리** (`280919b3ec08`, medium): 존재하지 않거나 종료된 채널 ID로 들어오는 요청을 무시/로깅.
- **채널 누수(Channel leak) 차단** (`289f508af6fe`, medium): 클라이언트의 비정상 종료 시 서버 쪽에 남은 댕글링 채널 리소스 해제.
- **초기 ACK 없는 Monitor 처리** (`2f4484889186`, medium): 모니터 생성 후 첫 ACK가 오기 전 발생하는 업데이트 이벤트 큐잉/드롭 로직.
- **GET_FIELD 마지막 연결 끊김 처리** (`5019744fa79c`, medium): 메타데이터 조회 중 클라이언트 연결이 끊겼을 때의 안전한 취소.
- **`autoExec=false` PUT 중 원격 오류 처리** (`70735383350b`, medium): 지연 실행 모드에서 발생하는 오류가 올바른 콜백으로 전파되도록 수정.
- **TX 버퍼 한계를 확인하여 스로틀링** (`8d58409481ef`, medium): 서버 송신 버퍼가 가득 찼을 때 이벤트를 버리거나 블로킹하는 배압(Back-pressure) 로직.

### 10-C. `pva-rs` — 와이어 프로토콜 & 디코딩 (Protocol & Decoding)

- **`SetEndian` 제어 메시지 올바른 처리** (`cce797263d1d`, high) — ⏭️ **ALREADY**: `proto/command.rs::ControlCommand::SetByteOrder`가 정의되어 있고 `server_native/tcp.rs:574`에서 handshake에 emit + 클라이언트 측에서도 수신 처리.
- **배열(Array) 디코드 버그 수정** (`cf91bc3033e2`, high): 특정 조건에서 가변 길이 배열 디코딩 크래시/잘림 해결.
- **디코드 오류 시 원격 `file:line` 정보 추출** (`e9ce80880d92`, high): 오류 응답에 포함된 상대방 디버그 위치 파싱.
- **`null` 문자열 디코딩** (`0356eee74037`, medium): 스칼라 문자열 타입에 `null`이 전달될 때의 기본값 처리.
- **`CMD_MESSAGE` 처리 수정** (`0eea8fd1c7e0`, medium): PVA 메시지 명령어 패킷의 올바른 파싱 및 라우팅.
- **자격 증명(Credentials) 디코드** (`7de1f7d32f63`, medium): 인증을 위한 Connection Request 내 자격 증명 블록 파싱 지원.

### 10-D. `pva-rs` — UDP / 비콘 (Beacon) & 기타

- **UDP RX 버퍼 오버플로 감지** (`a064677e3625`, high) — ⏸️ **DEFERRED (Linux-only + low-level)**: pvxs는 `SO_RXQ_OVFL` 소켓 옵션 + `recvmsg` cmsg에서 커널 드롭 카운터를 추출해 backlog 발생 시 디버그 로그 출력. Rust 측에 적용하려면 tokio `UdpSocket`이 노출하지 않는 `recvmsg+ancillary` 경로가 필요 — socket2/`nix` 직접 호출 + 자체 wake loop 구현, Linux 전용 분기. 추후 별도 PR.
- **클라이언트 비콘 수신 시작** (`acfba6469ed3`, high) — ⏭️ **ALREADY**: `client_native/search_engine.rs:598` `beacon_recv` future로 백그라운드 수신.
- **잘못된 스레드에서의 비콘 발송 경고** (`882a7720fb92`, medium) — ⚠️ **N/A**: Rust의 `Send`/`Sync` trait이 컴파일 시 보장.
- **서버 비콘 TX 최적화** (`cc5071cd22c4`, medium) — ✅ **DONE** (partial): (1) `SO_BROADCAST`는 `AsyncUdpV4::bind(port, broadcast=true)` 경로(`epics-base-rs::net::async_udp_v4.rs:667`)로 이미 설정. (2) 본 commit에서 PVA 비콘 emitter 루프(`server_native/udp.rs:176-`)에 `first_beacon` flag 추가 — 서버 시작 후 첫 beacon이 `beacon_period`(default 15s) 대기 없이 즉시 emit (pvxs `immediate={0,0}` libevent 타이머와 동일 효과). (3) beacon_destinations port는 caller(server-side bind)에서 결정되며 `SocketAddr`로 이미 정확한 port 보유. (4) 별도 `searchReply` vs `beaconMsg` buffer 혼용 버그는 C 한정 (Rust는 `build_beacon`이 별도 Vec 반환).
- **잘린 비콘(Truncated Beacon) 오류 무시** (`772cc5297cf8`, medium) / **반복적인 비콘 TX 오류 표시 제어** (`adcac746efff`, `91fed88cdd7f`) — ⏭️ **ALREADY**: 수신 측은 64KB UDP buffer 사용 (`crates/epics-pva-rs/src/client_native/search_engine.rs:593-594`)으로 truncation 자체가 발생하지 않음. 송신 측 TX 오류는 `beacon_send_errs: HashSet<SocketAddr>` per-destination dedup(`server_native/udp.rs:175,232-240`)로 first → warn, repeat → debug, recovery → remove 패턴 — pvxs의 first/change/recovery 의도와 동일.
- **비콘 정리 타이머 단순화** (`b33ea5df3113`, medium) — ⏭️ **ALREADY (structural)**: 원commit은 libevent 타이머를 one-shot에서 persistent로 바꾸고 manual `event_add` re-arm 제거. Rust 측은 `tokio::time::interval`(`search_engine.rs:586` `beacon_clean_tick`)이 본질적으로 periodic이므로 별도 re-arm 코드가 존재하지 않음.

---

**💡 추가 요약** 
기존 C++ `epics-base`에서 발생했던 수많은 치명적인 메모리 오염, 세그폴트, NULL 포인터 역참조 및 멀티스레딩 데이터 레이스 버그들(예: PR #496, #485, #25, #745 등)은 러스트의 **메모리 안전성(Ownership) 및 tokio 비동기 런타임 채택으로 인해 원천적으로 발생하지 않는(equivalent) 상태**임이 추가로 확인되었습니다.
