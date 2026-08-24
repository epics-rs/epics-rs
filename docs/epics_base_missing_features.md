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
- **FTDI 드라이버 (PR #88)** — 🔄 **PARTIAL (this session)**: 신규 `drivers/ftdi.rs` 모듈 — `FtdiConfig::parse(spec)` (vid/pid/serial/bitmode 파싱), `DrvAsynFtdiPort::new(spec)` 생성자, `PortDriver` trait 구현. Hardware path는 `ftdi-mpsse` Cargo feature opt-in 으로 분리 (default off → `connect()` 가 명시적 "feature not enabled" 에러 반환, 사이트 silent fail 방지). 9 단위 테스트 (vid/pid/serial 파싱 + 5 reject 케이스 + driver constructible + has_hw_support). 실제 libftd2xx/libusb 바인딩은 use case 등장 시 후속 PR.
- **IP 서버 포트의 `Bind` 인터페이스 및 `SO_REUSEPORT` 지원 (PR #148, #109)** — ✅ **DONE** (this session): 신규 모듈 `asyn-rs::drivers::ip_server_port::DrvAsynIPServerPort`. `IpServerConfig` 가 `host:port [TCP] [SO_REUSEPORT]` 파싱, IPv4/IPv6 bracket form 모두 지원. `accept_one()` 가 슬롯 테이블 (default 64 max_clients) 기반으로 incoming TCP 연결을 addr 슬롯에 할당. `read_octet`/`write_octet` 가 슬롯별 라우팅 + addr=-1 broadcast 지원. SO_REUSEPORT는 Linux/BSD `set_reuse_port(true)` 로 설정. 10 단위 테스트 (parser 5종 + e2e round-trip + slot cap + drop reuse + SO_REUSEPORT 두 번 bind).
- **`lsi`, `lso`, `printf` 레코드에 대한 `asyn` 매핑 (PR #104)** — ⏭️ **ALREADY**: `asyn-rs/adapter.rs` 가 `asynOctet` 인터페이스 (read/write)를 통해 String / CharArray ↔ EpicsValue 변환 처리 (`adapter.rs:411,462,773`). lsi/lso/printf 모두 String VAL 필드를 가지므로 `asynOctet` DTYP로 자동 wire-through. printf는 record-side format expansion이 별도 단계라 record 자체에서 해결.
- **단순 평균치 장치 지원 (Issue #30)** — ✅ **DONE** (this session): `interfaces::average` 모듈 신규 — `AsynInt32Average` / `AsynFloat64Average` 인터페이스 + `RingAverager<T>` 헬퍼 (capacity 기반 circular buffer, oldest 드롭, read-and-reset/peek/reset API). 5 단위 테스트.

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

- **직렬 포트 `Auto serial break` 기능 (PR #188)** — ✅ **DONE** (this session): `DrvAsynSerialPort::send_break(duration_tenths)` 와 `drain_output()` 추가. POSIX `tcsendbreak` / `tcdrain` 호출. BREAK 길이는 0.1s 단위 (Linux 정확값, BSD/macOS는 ≥0.25s).
- **`ASYN_TRACE_STATE` 마스크 비트 (PR #67)** — ✅ **DONE** (this session): `TraceMask::STATE = 0x0040` 추가. 회귀 테스트 `test_state_bit_value_and_disjoint`.
- **`asynMask`의 시프트 파라미터 (Issue #166)** — ⏭️ **ALREADY**: SHFT는 record-layer 책임 — `mbbiDirect`/`mbboDirect` 가 `shft: u32` 필드 보유, asyn-side `mask` 가 단순 bit 선택만 담당.
- **`setStringParam` NULL 포인터 안전성 (Issue #146)** — ⏭️ ALREADY: 러스트의 `Option<&str>` 모델 + 타입 시스템으로 구조적 방어. NULL deref 자체 발생 불가.
- **EOS(End-of-String) 설정자 블록 문제 (Issue #103)** — ⏭️ **ALREADY**: `interpose/eos.rs` 의 `set_input_eos`/`set_output_eos` 가 atomic Mutex 안에서 update — C 의 thread blocking 패턴 부재.
- **장치 드라이버로의 파라미터 변경 알림(Notification) 방향성 버그 대조 (Issue #46)** — ⏭️ **ALREADY** (verified): bidirectional notification은 이미 완비. driver→record는 `interrupt.rs::Interrupt` + `call_param_callbacks` 가 처리, record→driver는 trait method `write_int32`/`write_float64`/...의 default impl이 `set_*_param` + `call_param_callbacks` 호출로 callback 발화. driver internal poll task → records는 `set_int32_param` + `call_param_callbacks` 호출로 동등 처리. 양방향 알림 갭 부재.
- **`drvAsynIPPort` 읽기 타임아웃 시 연결 종료 옵션 (PR #6)** — ⏭️ **ALREADY**: `IpPortConfig::disconnect_on_read_timeout: bool` (`drivers/ip_port.rs:364`), read 경로 L649 에서 timeout 시 자동 disconnect. 회귀 테스트 `test_disconnect_on_read_timeout`.
- **`asynSetTrace*Mask`의 문자열 옵션 파싱 (PR #76)** — ✅ **DONE** (this session): `TraceMask::from_symbolic`, `TraceIoMask::from_symbolic`, `TraceInfoMask::from_symbolic` — case-insensitive bit names + ASYN_-prefixed aliases + 숫자 토큰 지원. 7 단위 테스트.

---

## 6. 로드맵 상의 의도적 제외 사항(By-Design Gaps) 및 코드베이스 TODO
과거 커밋이나 이슈가 아닌, `epics-rs`의 아키텍처 철학(`ROADMAP.md`) 및 코드베이스 내부의 `TODO` 주석을 통해 파악된 마지막 미구현/제외 항목들입니다.

### 의도적 제외 사항 (Out-of-Scope)
C++ `epics-base`에는 존재하지만 러스트 생태계의 특성상 **의도적으로 구현하지 않기로 한(By-Design)** 항목들입니다.
- **RTEMS 및 VxWorks 운영체제 지원**: `epics-rs`는 Linux(및 PREEMPT_RT), macOS, Windows 등 Tier-1/2 호스트 OS에 집중하며, 임베디드 실시간 OS 지원은 아예 스코프에서 제외되었습니다. (해당 용도는 C++ `pvxs` 권장) 단, RTEMS 6에서는 pvxs도 배포 상태 그대로는 동작하지 않습니다 — `src/evhelper.cpp:183`의 RTEMS-5 시절 `kqueue` 회피 코드를 제거해야 정상 서비스됩니다 (측정 및 원인: `doc/rtems-scope-b-session-handoff.md` §5.3).
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
- **`aai` 레코드의 pass-1 디바이스 초기화 지원** (`1c566e21`/`6754404d`, 2021) — ⏭️ **ALREADY**: `Record::init_record(pass)` trait 메소드가 0/1 두 패스 호출을 받음 (`ioc_builder.rs:251-256`). aai 디바이스 init이 pass-1에 필요하면 `if pass == 1 { … }` 분기로 처리.
- **배열 레코드의 `BPTR` 필드 런타임 변경 지원** (`2340c6e6`, 2021) — ⚠️ **N/A (design diff)**: C `BPTR` 은 raw `void *` buffer pointer 노출 — 디바이스 지원이 직접 buffer를 swap. Rust `WaveformRecord::val: EpicsValue::*Array(Vec<…>)` 는 ownership-managed; runtime swap은 `set_val(new_buf)` 단일 entry point.
- **`compress` 레코드 평균 알고리즘 버그 수정** (`11a4bed9`, 2022) — ⏭️ **ALREADY**: 9-B 동일 항목 — `compress.rs::flush_accum` 의 mean 계산이 정확.
- **`lsi`/`lso` 레코드의 `SIZV` 필드 크기 계산 버그** (`4966baf4`, 2024) — ⏭️ **ALREADY**: 9-B 동일 — `i32` SIZV.
- **`arrRecord`의 `cvt_dbaddr()` 동작 통일** (`eeb198db`, 2020) — ⚠️ **N/A**: arrRecord (legacy array record) 가 epics-rs 미구현. 사용 사례 적음.
- **`dbConstAddLink`의 DBR 타입 경계 검사** (`552b2d17`, 2021) — ⏭️ **ALREADY**: `record/link.rs::parse_link_v2` 의 `Constant(String)` 분기에서 타입 변환은 사용 시점에 `convert_to(target_type)` — 경계 검사는 `EpicsValue::convert_to` 안에서 강제.
- **호스트명 최대 길이 제한 제거** (`87acb98d`, 2022) — ⏭️ **ALREADY**: ADDR_LIST 파서 (`server/addr_list.rs`)는 `&str` slice + `split_whitespace`로 처리, 고정 길이 버퍼 부재.
- **`iocinf.cpp` 호스트명 버퍼 오버플로** (`a8e8d22c`, 2022) — ⚠️ **N/A (eliminated)**: Rust `String` 동적 길이, 32-byte 고정 버퍼 패턴 부재.
- **`postfix()` 함수의 널 포인터 역참조** (`60fa2d31`, 2023) — ⚠️ **N/A (eliminated)**: calc engine `crates/epics-base-rs/src/server/calc/postfix.rs` 는 `&mut Vec<Op>` 받아 fail-fast — null deref 가능성 부재.
- **`dbEvent` 잔여 이벤트 카운트(`eventsRemaining`) 오계산** (`e1c1bb8b`, 2023) — ⚠️ **N/A (eliminated)**: dbEvent worker queue 자체가 부재 — `mpsc::Receiver::len()` 이 정확.
- **`callbackSetQueueSize` 상한 검사** (`baa4cb54`, 2025) — ⚠️ **N/A (design diff)**: epics-rs는 C의 `callback.c` 큐 시스템을 사용하지 않고 tokio 런타임의 `mpsc::channel` / spawn 으로 콜백 워크를 처리. `callbackSetQueueSize` 등가 API 자체가 없음. 음수/0 큐 사이즈 입력 검증이 필요한 진입점이 부재.
- **`CHAR` 배열 출력 시 비출력 문자 이스케이프** (`dc70dfd6`, 2022) — ✅ **DONE**: `cmd_dbgf`가 `EpicsValue::CharArray` 케이스에서 신규 `escape_char_array_for_dbgf` 헬퍼로 C 스타일 escape 후 큰따옴표 wrap (`"..."`). short form: `\n` `\t` `\r` `\\` `\"` `\a` `\b` `\f` `\v`, 그 외 non-printable 및 high-bit (0x7f..=0xff)는 `\xNN`. 다른 EpicsValue 타입은 기존 Display 그대로. Unit 테스트 3종.

---

### 7-C. 런타임 수명주기 / 셧다운 (Lifecycle, 112건 → 핵심 점검 항목)
- **`CA Repeater`를 프로세스 실행 실패 시 스레드로 폴백** (`08b741ed`, 2021) — ⏭️ **ALREADY**: epics-ca-rs는 `repeater::ensure_repeater` 가 in-process repeater task를 spawn (`repeater.rs:265`) — 외부 프로세스 실행 실패 자체를 회피. C 의 "fork failed → fallback to thread" 패턴 등가가 기본 동작.
- **`caRepeater`에 `-d` 디버그 옵션 추가** (`e2717521`, 2026) — ✅ **DONE** (cycle 15, commit `b3e0fe0`): `ca-repeater-rs -d`/`-dd` clap arg + `run_repeater_with_debug(level)`.
- **`iocsh` 명령어에 `iocshSetError()` 전파** (`144f9756`, 2024) — ✅ **DONE**: `IocShell::execute_script` / `execute_script_with_macros`가 라인별 `Err`를 `last_err`로 캡처하여 스크립트 종료 시 종합 Err 반환 (=`iocshSetError` 의 비-제로 exit code 등가). 본 commit에서 `dbLoadRecords`가 add_record 거부 시 `Ok(Continue)`로 swallow하던 케이스(`commands.rs:1000-1002`)를 `Err(e)` 반환으로 수정 + duplicate name regression 테스트 추가.
- **`iocsh` 인자 파싱 버그 수정** (`3dbc9ea2`, 2023) — ⏭️ **ALREADY**: 원버그는 `char quote = EOF (-1)` 센티넬이 VxWorks의 unsigned char에서 `0xFF`로 wrap되어 입력의 0xFF 바이트와 충돌. Rust tokenizer (`crates/epics-base-rs/src/server/iocsh/registry.rs`)는 `let mut in_quotes: bool` 로 양자 상태를 유지하므로 sentinel 충돌 가능성 자체가 없음. 추가로 `find_closing_paren`/`split_comma_args`/`split_space_args` 3곳 모두 동일 패턴.
- **`casStatsFetch()` RSRV 미초기화 시 안전성** (`7a6e11ca`, 2026) — ⚠️ **N/A**: C는 전역 `clientQlock` / `rsrvCurrentClient`가 미초기화 NULL 상태에서 stats 조회 시 NULL deref. Rust `ServerStats`는 `Arc<ServerStats>` (`crates/epics-ca-rs/src/server/ca_server.rs:337+`) 이며 `Default::default()`로 항상 0 초기화 + atomic 카운터 사용 — "미초기화 NULL"이라는 상태 자체가 존재하지 않음. RSRV disabled 시에도 `Arc<ServerStats>`는 valid한 0-stats를 반환.
- **`dbGet`의 루프-안전 래퍼** (`dac620a7`, 2024) — ⚠️ **N/A (design diff)**: C dac620a7는 `dbDbGetControlLimits/GraphicLimits/AlarmLimits`가 같은 필드를 가리키는 link 따라가다가 무한 재귀에 빠지는 케이스를 `DBLINK_FLAG_VISITED` 플래그로 차단. epics-rs는 메타데이터(HOPR/LOPR/DRVH/DRVL/alarm limits)를 별도 link traversal로 가져오지 않고 record 필드에서 직접 읽기 때문에 재귀 경로 자체가 부재. process chain 재귀는 `visited: HashSet<String>`로 별도 보호됨.
- **`NAMSG` 알람 문자열 필드를 `NSTAT`/`NSEV`와 함께 초기화** (`8483ff95`, 2024) — ⏭️ **ALREADY**: `rec_gbl_reset_alarms`(`crates/epics-base-rs/src/server/recgbl.rs:121`)가 `common.amsg = std::mem::take(&mut common.namsg)`로 promote 직후 namsg를 자동 클리어. `reset_alarms_transfers_amsg_and_clears_namsg` 테스트로 회귀 방어.
- **`lset::getAlarmMsg()` API** (`5143c71a`, 2020) — ⏭️ **ALREADY**: `processing.rs::process_record_with_links_inner` 의 single-INP MS path가 `read_link_with_alarm` 으로 source의 alarm.amsg 추출, link_alarms 리스트로 전파. 별도 lset trait method 없이 동등 결과.
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
- **타이머 조기 만료(Early expiry) 버그** (`01360b2a`, 2022) — ⚠️ **N/A (eliminated)**: tokio timer wheel은 monotonic — early expiry 가능성 부재. CA `SearchTimer` 등가는 `interval(NORMAL_TICK)`.
- **NaN/Overflow 타임아웃 값 처리** (`1655d68e`, 2022) — ✅ **DONE (analog)**: 원커밋은 RTEMS osdEvent 한정 (Rust 비대상)이지만 동일 정신: `epics_ca_rs::cli::timeout_duration` (`crates/epics-ca-rs/src/cli.rs`)이 NaN/±Inf/0/음수를 `DEFAULT_CLI_TIMEOUT_SECS=1.0`으로 클램프해 `Duration::from_secs_f64` panic을 차단. `env_default_timeout`도 같은 가드. 4개 CA CLI(caget/caput/cainfo/camonitor) 모두 `timeout_duration` 경유. PVA 측은 `epics_pva_rs::cli::timeout_duration` (default 5.0s) 추가, `pvcall-rs`에 적용. `pvlist-rs`는 `0`=wait-forever 의미를 보존하기 위해 `is_finite() && > 0.0` 가드만 적용 (Inf/NaN도 wait-forever). 테스트 5종.
- **`EPICS_CLI_TIMEOUT` 환경 변수** (`1d056c6f`, 2022) — ⏭️ **ALREADY**: `epics_ca_rs::cli::env_default_timeout` (`crates/epics-ca-rs/src/cli.rs:10`)이 EPICS_CLI_TIMEOUT 환경변수를 읽어 unset/unparseable 시 1.0s fallback. `caget`/`caput`/`camonitor`/`cainfo` 4개 binary 모두 `.unwrap_or_else(env_default_timeout)` 패턴으로 적용. clap의 `-w` parse 실패 시 즉시 종료(C의 silent-revert와 달리 안전).
- **단조시간(Monotonic Clock) 기반 CA 타임아웃 통일** (`f1cbe93b`, 2020) — ⚠️ **N/A (eliminated)**: 모든 epics-rs 타임아웃이 `tokio::time::Instant` (monotonic) 기반. C의 wall-clock vs monotonic 분기 자체가 부재.
- **macOS 단조 시계 해상도 버그** (`3506d115`, 2020) — ⚠️ **N/A (eliminated)**: tokio가 OS 기본 monotonic clock (macOS는 `mach_absolute_time`)을 사용; epics-rs 코드는 직접 `clock_gettime` 호출 없음.

---

### 7-E. 동시성 / 데이터 레이스 (Race, 46건 → Rust로 대부분 해결됨)
아래 C++ 커밋들은 **Rust의 소유권 모델, `Arc<Mutex<>>`, `tokio` 비동기 런타임**으로 인해 구조적으로 해결된 사례들입니다. 단, 일부는 논리적 동시성 이슈이므로 교차 검증이 필요합니다.

- **`concurrent db_cancel_event()` 데드락** (`9f868a10`, 2023) — ⚠️ **N/A (eliminated)**: subscription cancel은 `RecordInstance::remove_subscriber(sid)` 단일 entry, write lock 1회 → 즉시 release — lock cycle 부재.
- **`db_create_read_log`/`dbChannelGetField` 잠금 누락** (`9f788996`, 2023) — ⚠️ **N/A (eliminated)**: 모든 record read는 `RwLock<RecordInstance>` read lock 안에서. lock-less read 경로 부재.
- **`epicsThreadOnce()` 경쟁 조건** (`5507646c`, 2023): → Rust의 `std::sync::Once`로 원천 해결됨.
- **`ipAddrToAsciiGlobal` 공유 스크래치 버퍼 레이스** (`82338657`, 2023) — ⚠️ **N/A (eliminated)**: `SocketAddr::to_string()` 사용; 공유 스크래치 버퍼 패턴 부재.
- **`epicsMessageQueue` 스레드 노드 미초기화** (`a7a56912`, 2023): → Tokio `mpsc::channel`로 대체.
- **`dbCaSync()` 수정** (`e9e576f4`, 2021) — ⚠️ **N/A (eliminated)**: dbCa link 동기화는 `Arc<RwLock<...>>` + `LinkSet::is_connected` 폴링 — race-free.
- **`CLOCK_MONOTONIC_RAW` 제거** (`597393a8`, 2019) — ⚠️ **N/A (eliminated)**: tokio `Instant` 가 OS 기본 monotonic clock 사용.
- **우선순위 역전 뮤텍스(PI Mutex)** (`5a8b6e41`, 2020) — ✅ **DONE** (this session): `runtime::sync::PriorityInheritanceMutex<T>` 타입 alias. `linux-rt` Cargo feature 활성 시 `pthread_mutex_t` + `PTHREAD_PRIO_INHERIT` 사용, 비-RT/non-Linux는 `parking_lot::Mutex` fallback. `is_pi_mutex_active()` 런타임 진단. 회귀 테스트 2종.

---

### 7-F. 흐름 제어 / 큐 (Flow-Control, 7건)
- **`dbnd` 필터의 알람/프로퍼티 이벤트 통과** (`446e0d4a`, 2021) — ⏭️ **ALREADY**: `filters/dbnd.rs::apply` 가 `posting_mask.intersects(EventMask::ALARM | EventMask::PROPERTY)` 시 unconditional pass-through.
- **`dbEvent` 큐 사이즈 조정** (`c8e5deca`, 2019) — ⏭️ **ALREADY**: `mpsc::channel(64)` per-subscriber queue (`record_instance.rs:1771`). 환경변수로 조정 가능 (cap 변경 사용 사례 없음).
- **`callbackParallelThreads` 비율(%) 지정 지원** (`fe39a007`, 2026) — ⚠️ **N/A (design diff)**: epics-rs는 C의 `callback.c` 큐 시스템과 그 `callbackParallelThreads` iocsh 명령을 갖지 않음 (tokio 런타임이 callback work를 처리). 백분율 인자 파싱이 적용될 진입점 자체가 부재. tokio worker thread 수는 별도 `tokio::runtime::Builder::worker_threads` 설정으로 제어, 백분율 입력이 필요해질 때 iocsh 명령 신설 시 동등하게 처리.
- **CPU 과다 보고 방지** (`556de06f`, 2026): → 섹션 3의 기존 항목과 동일 (PR #788).
- **필터 내 `dbGet` 통과 경로** (`17a8dbc2`, 2020) — ⏸️ **DEFERRED**: 9-C 동일 항목 — DB link read path의 filter 적용은 별도 작업.

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

- **`EPICS_IOC_IGNORE_SERVERS` 환경 변수** (`6efe2924`, 2017) — ✅ **DONE** (commit `8615bb4`): ADDR_LIST 파싱 / SEARCH 응답 / beacon 수신 3개 경로에서 quarantine IP 필터.
- **`EPICS_CA_MCAST_TTL` 환경 변수** (`f2a1834d`, 2017, 3.16) — ✅ **DONE** (commit `ae277d1`): `runtime::net::ca_mcast_ttl()` + `AsyncUdpV4::set_multicast_ttl_v4` + CA server beacon/UDP responder/client search 적용.
- **rsrv: 최대 배열 바이트(max array bytes)를 초과하는 큰 배열 지원** (`3009f88f`/`85b6b5c5`, 2017) — ⏭️ **ALREADY**: `epics-ca-rs` MAX_PAYLOAD_SIZE 1MB cap (transport.rs); 서버는 이 cap 안에서 동적 length 지원. `EPICS_CA_MAX_ARRAY_BYTES` 등가 cap 환경변수는 추후 추가 가능 (사용 사례 없음).
- **rsrv 멀티 인터페이스 바인딩 재구성** (`15307c4d`, 2016) — ⏭️ **ALREADY**: CA 서버 `run_udp_search_responder` (`server/udp.rs:33-39`)가 `intf_addrs: Vec<Ipv4Addr>` 받아 per-interface task 별도 spawn. 다중 NIC 바인딩은 처음부터 cleanly architected.
- **camonitor 데이터 타입 변경 처리** (`16877577`, 2020, 3.15.7) — ✅ **DONE** (this session): `ConnectionEvent::NativeTypeChanged { previous, current }` variant 추가. `client/mod.rs::TransportEvent::ChannelCreated` 핸들러가 새 native_type 과 직전 값을 비교, 다른 경우 변경 이벤트 broadcast. camonitor/archiver 등 소비자가 per-type decoder 재구축 가능.
- **mcast loopback 소켓 옵션 활성화** (`98504d1c`, 2016) — ⏭️ **ALREADY**: tokio UdpSocket의 `set_multicast_loop_v4(true)` 명시적 호출 (loopback_mcast.rs).
- **`casr()` 출력 개선** (`1c1eb030`, 2016) — ⚠️ **N/A (eliminated)**: epics-rs `casr` 등가 명령은 iocsh `casStats` — 출력 형식은 처음부터 cleanly designed.
- **`EPICS_NO_CALLBACK` 환경 변수** (`75a1b823`, 2019) — ⚠️ **N/A (design diff)**: callback 시스템 없음 (tokio mpsc + spawn). disable 대상 부재.
- **`CASDEBUG` 환경 변수를 `iocsh`에 노출** (`546df1c1`, 2017) — ⏭️ **ALREADY**: `RUST_LOG=epics_ca_rs::server=debug` 환경변수 + tracing-subscriber로 더 fine-grained 제어 가능.

---

### 8-B. 레코드 타입 / 필드

- **`subArray` 레코드 개선 및 소프트 디바이스 지원** (`d1af6637`, 2017) — ⏭️ **ALREADY** (commit `a02c310` + cycle 1): `WaveformRecord` 가 SubArray kind 지원, INDX/MALM 슬라이싱 + 빈 source / partial tail / MALM cap 모두 처리.
- **`int64in`/`int64out` 레코드의 모니터 델타 버그 수정** (`3091f7c5`, 2021) — ⏭️ **ALREADY**: int64in/out도 deadband 검사가 `check_deadband_ext` 공통 코드 — i64 → f64 변환 후 분기. 64bit/32bit 분기 자체가 부재.
- **`PUTF`를 통해 `DB_LINK` 및 `RPRO` 비동기 전파** (`a4fcd229`, 2018) — 🔄 **PARTIAL (verified)**: `processing.rs::dispatch_cp_targets` 가 RPRO만 전파 (`tg.common.rpro = true` L1472). PUTF는 PR #3fb10b6 fix로 의도적으로 CP target에 전파 안 함 (CP-driven targets must keep PUTF=false). DB_LINK OUT-driven 경로 (write_db_link_value)에서의 PUTF forward는 미구현. 현재 사용 사례에서는 RPRO 전파만으로 충분하나, OUT-link PUTF 전파가 필요한 사용 사례 등장 시 별도 PR.
- **`dbCa` CP 링크 업데이트 시 `PUTF`/`RPRO` 설정** (`a4bc0db6`, 2024) — 🔄 **PARTIAL (verified)**: 위 항목과 동일 — RPRO만 전파, PUTF는 의도적 보류.
- **`scanOnceCallback()` 완료 콜백 지원** (`2ba2b90b`/`bbbf0541`, 2015) — ⏭️ **ALREADY**: tokio `JoinHandle::await` 또는 `oneshot::channel` 패턴으로 등가. `process_record_with_links` 자체가 await 가능.
- **`dbScan`: I/O Intr 목록 직접 스캔 지원** (`7d50f62a`, 2015) — ⏭️ **ALREADY (different mechanism)**: 별도 `IoIntrIndex` 자료구조 없음. `ioc_app.rs::setup_io_intr` (L629)가 모든 record를 walk하면서 `ScanType::IoIntr` 인 것만 picked, 각 record의 `io_intr_receiver()`로부터 device-driven push event를 받아 process_record 호출. C 의 직접 list iter 등가는 record map walk + scan filter로 달성.
- **`dbCa`: 가변 길이 배열 구독** (`b2716f0a`, 2015) — ⏭️ **ALREADY**: CA 링크 monitor가 NORD 변화 시 `count` 자동 조정. count=0 semantic도 처리.
- **`aSub` 레코드 INAM 변경 시 출력 처리** (`2af98c33`, 2017) — ⏸️ **DEFERRED (verified gap)**: `asub_record.rs::put_field("INAM")` (L681) 가 단순히 `self.inam = s` 만 수행, subroutine registry 재조회 + init function 재호출은 미구현. 현재 사용 사례에서 runtime INAM 변경이 드물어 미구현 — 필요 시 별도 PR.
- **`aSub` 레코드의 올바른 데이터 복사량** (`52787995`, 2017) — ⚠️ **N/A (eliminated)**: aSub 데이터 복사는 `Vec::clone()` — 길이 자동 매치, off-by-one 불가.
- **`asTrapWrite`에 Put 데이터 제공** (`c5ded306`, 2015) — ⏭️ **ALREADY (different mechanism)**: `epics_ca_rs::audit::AuditLogger` 가 PV name + user + host + method + value를 JSON 기록 — asTrapWrite 등가 확장 정보 모두 포함.
- **`xRecord` 디바이스 지원** (`b9cbf7a3`, 2015): 모든 타입의 디바이스를 연결할 수 있는 범용 `xRecord`.

---

### 8-C. DB/링크 시스템

- **JSON Links 시스템 도입** (`7edc0c67`, 2016) — ⏭️ **ALREADY (partial)**: `record/link.rs::try_parse_json_link` 가 `{const:..}` / `{calc:..}` 등 JSON link form 파싱. lnkCalc 의 expression 평가는 calc engine 재사용. lnkConst/lnkDbState 처리.
- **`lnkCalc` 링크 타입의 타임스탬프 지원** (`e3c9d590`/`20404003`, 2017/2018) — ✅ **DONE** (this session): 신규 `ParsedLink::Calc(CalcLink { expr, args, time_source })` variant + serde_json 기반 파서 (`{calc:{expr:"...",args:[...],time:"A"}}`). `read_link_value_soft` + `read_link_value` 양쪽 경로에 calc 평가 분기 추가. `evaluate_calc_link` 가 다중 input PV fetch → calc engine A.. 바인딩 → eval. `db_get_time_stamp_tag` 가 `ParsedLink::Calc` arm 에서 `time_source` letter 가 가리키는 input record 의 `common.time`/`common.utag` 를 반환하고, TSE=-2 + constant-TSEL 게이트 아래에서 읽는 record 가 둘 다 채택한다 (`lnkCalc.c:580-581`). 회귀 테스트 `calc_link_adopts_its_time_inputs_stamp.rs` (경계별 6 케이스) + `test_lnk_calc_parses_and_evaluates` (parse + `CALC_NARGS` cap + read-path eval).
- **`dbLink`의 필드 타입을 `DOUBLE`로 반환** (`9813fa64`, 2015) — ⏭️ **ALREADY**: `read_link_value_soft` 가 source의 native type 반환, caller가 `to_f64()` 또는 `convert_to(target_type)` 명시.
- **링크 필드의 긴 문자열 버퍼 크기 확장** (`19447dc7`, 2016) — ⚠️ **N/A (eliminated)**: INP/OUT 은 `String` (동적 길이) — 128-byte fixed buffer 한계 없음.
- **`dbPutStringNum("", ...)` 을 오류로 처리하지 않음** (`0821c8c4`, 2016) — ⚠️ **N/A (semantic diff)**: `parse_string_to_f64` (`types/value.rs:754`) 가 빈 문자열을 `None` 으로 반환 → caller (put_field) 가 `Err` 반환. 12cfd41 "empty array → Err" 와 동일한 fail-fast 정책. C 0821c8c4 는 "silently accept and use default" — 우리는 의도적으로 stricter (silent garbage 대신 명시적 거절).
- **`dbLinkDoLocked()` 지원** (`d2db634e`, 2017) — ⏭️ **ALREADY**: 모든 link 작업이 record write lock 안에서 수행. `dbLinkDoLocked` 등가는 자동.
- **`iocshFindCommand()` API** (`9d7c4434`, 2017) — ⏭️ **ALREADY**: `iocsh/registry.rs::CommandRegistry` 가 `HashMap<String, Box<dyn CommandFn>>` — `commands.get(name)` 으로 조회.
- **`dbRecordsAbcSorted`: 알파벳 순 레코드 목록** (`a32faa57`, 2016) — ⏭️ **ALREADY**: iocsh `dbl` 가 records를 sort 해서 출력 (`commands.rs::cmd_dbl`).
- **`dbStatic`: 알파벳 정렬 옵션(opt-in)** (`336bd656`, 2016) — ⚠️ **N/A**: 항상 sort 출력 (느린 환경 없음 — 1만 레코드 sort도 마이크로초). opt-in 토글 불필요.
- **빈 배열(`""`) 입력 링크 허용** (`ec650e8c`, 2022) — ⏭️ **ALREADY**: `parse_link_v2` 가 `s.is_empty()` 또는 `""` quoted를 `ParsedLink::None` 반환 — 빈 입력 허용.

---

### 8-D. iocsh / 런타임 / 환경

> 본 세션 일괄: **⏸️ DEFERRED** (단, `dbServerStats`는 🔄 PARTIAL `ac92e3e` — 섹션 2 참조).

- **iocsh 스크립트 include 시 echo 비활성화 옵션** (`0fd07d16`, 2016) — 🔄 **PARTIAL (verified)**: `IocShell::execute_script` (L177) 가 매 라인마다 `println!("{line}")` 로 echo (C 기본 동작과 동일). C 0fd07d16 이 추가한 "no-echo" 옵트아웃은 미구현 — 환경변수 또는 `IocShell` 옵션으로 추가 가능.
- **`dbStopServers()` 를 `iocShutdown()`에 포함** (`a9393242`, 2017) — ⏭️ **ALREADY**: SIGTERM/SIGINT 핸들러가 모든 spawn된 task drop 시 CA/PVA server task 자동 cleanup.
- **`readline`을 `epicsExit()`에서 정리** (`444b89f5`, 2015) — ⏭️ **ALREADY**: rustyline `Editor::drop` 자동 cleanup.
- **`EPICS_TZ` 환경 변수로 표준화** (`b0db6568`, 2019) — ⚠️ **N/A**: 원 commit은 RTEMS `rtems_init()`에서 `EPICS_TIMEZONE` 대신 `EPICS_TZ`를 읽도록 변경. Rust는 RTEMS 비대상 + `chrono::Local` 등이 OS POSIX `TZ` 환경변수를 자동 사용하므로 EPICS-namespaced timezone env var를 별도로 다룰 진입점이 없음.
- **`generalTime`의 이벤트 번호 >= 256 지원** (`215c5d95`, 2018) — ⏭️ **ALREADY**: `runtime::general_time::get_event(i32)` 가 i32 받아 256+ 코드 지원.
- **`osiClockTime` 동기화 훅 지원** (`5cfff383`, 2019) — ✅ **DONE** (this session): `runtime::general_time::register_clock_sync_hook(F)` + `notify_clock_sync(t)` API 추가. 시간 소스 (PTP/NTP/GPS PPS) 가 fresh sync 받았을 때 등록된 콜백을 registration order 로 발화. ratchet semantic은 영향 없음 — pure notification channel. 회귀 테스트 `sync_hooks_fire_in_registration_order`.
- **`epicsTime` UTC `struct tm` 전체 변환** (`37024011`, 2016) — ⚠️ **N/A (eliminated)**: epics-rs는 C `struct tm` API를 노출하지 않음. 시간 표현은 `std::time::SystemTime` (epoch ns 정밀도) — call site 가 필요 시 `chrono` crate (Cargo.toml에 dep으로 등록)을 사용해 `DateTime<Utc>` 변환. C 의 timezone-aware struct tm 변환 버그가 발생할 수 있는 surface 자체가 부재.
- **`envGetBoolConfigParam` 함수** (`f837add8`, 2016) — ⏭️ **ALREADY**: `runtime::env::get_bool` 구현.
- **`iocsh`에 등록된 변수/함수 목록 조회 API** (`daad3c69`, 2016) — ⏭️ **ALREADY**: `CommandRegistry` enumeration + auto `help` 명령.
- **`dbServerStats()` API** (`bcc6cb96`/`350570134`, 2025, PR #592) — ✅ **DONE**: 섹션 2 동일 (commit `ac92e3e`).
- **iocsh ANSI 컬러 출력** (`c0da3dd`, 2025) — ✅ **DONE** (this session): `IocShell::run_repl_interactive` 가 cyan 프롬프트 (`\x1b[36m...epics> \x1b[0m`), 에러는 bold-red `\x1b[1;31mError:\x1b[0m ...`. rustyline `\x01...\x02` 브래킷으로 prompt-width tracking 보존. `NO_COLOR=1` (https://no-color.org) 및 `EPICS_RS_IOCSH_NO_COLOR=1` opt-out 지원. 회귀 테스트 2종 (format_error / use_ansi_color env vars).

---

### 8-E. 스캔 / 이벤트 / 콜백

> 본 세션 일괄: **⏸️ DEFERRED**.

- **`epicsCallback` 타입 도입** (`00a974ce`/`73fec881`, 2018/2019) — ⚠️ **N/A (eliminated)**: callback system 자체가 부재 (tokio mpsc + spawn). 타입-안전 callback wrapper는 Rust closure로 자동.
- **콜백 큐 상태(callback queue status) 노출** (`59ec8d89`, 2018) — ⚠️ **N/A**: callback queue 부재.
- **`EPICS_NO_CALLBACK` 옵션** (`75a1b823`, 2019) — ⚠️ **N/A**: 8-A 동일.
- **dbScanPassive를 `dbDbLink.c`로 이동** (`7626856a`, 2018) — ⚠️ **N/A (design diff)**: epics-rs 는 link/scan 분리가 다름 — `processing.rs::write_db_link_value` 가 PP semantic 처리. C 의 architectural reorganization 등가가 부재.
- **주기 스캔 속도 보호** (`49e0e23f`, 2017) — ⚠️ **N/A**: scan period가 enum (`ScanType::Period5s`/`Period1s`/...) 으로 정의 — 너무 빠른 값 입력 자체가 불가.
- **dbCa: `dbCaPutLinkCallback`의 초기화 버그** (`c0cf25ee`/`3501fda4`, 2015) — ⚠️ **N/A (eliminated)**: dbCa Put callback이 `EpicsValue` Vec 전달 — 길이 자동 매치, off-by-one 불가.

---

### 8-F. 필터 시스템

> 본 세션 일괄: **⏸️ DEFERRED** — Section 1의 "서버 측 채널 필터" 항목 종속. 필터 프레임워크 자체가 deferred이므로 그 안의 모든 필터별 버그도 자동으로 deferred.

- **`arr` 필터의 wrap이 `capacity` 기준으로 동작** (`840da801`, 2016) — ⏭️ **ALREADY**: `filters/arr.rs` 가 array length 기반 wrap (capacity ≥ length). C wrap-on-capacity 버그가 부재.
- **`sync` / `unless` 모드 필터의 메모리 누수** (`8ff6ce48`, 2019) — ⚠️ **N/A (eliminated)**: epics-rs sync 필터 (`filters/sync.rs`) 가 `Mutex<Option<MonitorEvent>>` 사용 — `Drop` 자동, 누수 불가.
- **`decimate` 필터의 드롭된 field-log 누수** (`f79c69f0`, 2019) — ⚠️ **N/A (eliminated)**: `filters/dec.rs` 가 dropped event를 `MonitorEvent` Drop으로 자동 해제. 누수 불가.

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

- **`histogramRecord` wdog 콜백이 `VAL` 대신 `bptr`로 이벤트 발송** (`4a0f488`, medium) — ⚠️ **N/A (eliminated)**: histogram record 가 epics-rs 미구현. 추후 구현 시 `notify_field("VAL", …)` 패턴 강제 — `bptr` raw pointer 개념 자체가 부재.
- **영(zero)원소 배열 읽기에 대한 고유 오류 코드** (`5d808b7`+`3b3261c`, medium) — ⏭️ **ALREADY (post-revert)**: 섹션 5 보강 참조. upstream이 `S_db_emptyArray`를 revert하여 다시 `S_db_badField` 사용, Rust `CaError::InvalidValue`가 동일 의미.
- **`.DTYP` 없는 레코드 타입에서 `DTYP` 조회 시 크래시 대신 빈 문자열** (`6e7a715`, medium) — ⏭️ **ALREADY**: `CommonFields::dtyp: String` (default empty), `get_common_field("DTYP")` 항상 `Some(String)` 반환. 빈 문자열도 valid String, NULL deref 불가.
- **`get_enum_strs` 포인터 산술이 `_FORTIFY_SOURCE=3`에서 경고** (`979dde8`, medium) — ⚠️ **N/A (eliminated)**: C 매크로 raw pointer arithmetic. Rust enum strings는 `Vec<String>` + index — bounds check 강제, 보안 검사 대상 부재.
- **`lsi`/`lso` `SIZV` 필드가 32767에서 오버플로** (`e5b4829`, medium → 7-B 항목 보강) — ⏭️ **ALREADY**: `LsiRecord::sizv`/`LsoRecord::sizv` 가 `i32`로 선언, 내부 buffer는 `String`으로 길이 제한 없음.
- **`compressRecord` `compress_scalar` 평균 계산 버그** (`11a4bed`, partial → 7-B 항목 보강) — ⏭️ **ALREADY**: `compress.rs::flush_accum` 의 mean 분기가 `accum.iter().sum::<f64>() / accum.len() as f64` — divide-by-zero 가드 + Rust f64 산술.
- **`compressRecord` `compress_array`: `PBUF=YES`일 때 부분 버퍼 거부** (`84f4771`, partial) — ⏭️ **ALREADY** (commit `52427bc`): `push_array` PBUF=YES 시 trailing partial chunk 즉시 emit.
- **`dbPutConvertJSON`: 빈 JSON 문자열이 yajl에 전달되어 파싱 오류** (`ec650e8`, partial) — ⚠️ **N/A (design diff)**: epics-rs는 yajl 미사용; serde_json. 빈 문자열은 `serde_json::from_str("")` Err → `?`로 propagate. silent garbage 차단.
- **`epicsNAN`/`epicsINF`를 모든 플랫폼에서 진정한 const로** (`5485ada`, medium) — ⚠️ **N/A (eliminated)**: Rust `f64::NAN`, `f64::INFINITY`, `f64::NEG_INFINITY` 모두 컴파일 타임 const.
- **`DBF_CHAR` waveform 필드에 대한 상수 링크 문자열 초기화 실패** (`b36e526`, medium) — ⏭️ **ALREADY**: `WaveformRecord::put_field("VAL")` 의 `(EpicsValue::String(s), 1 | 2) => CharArray(s.as_bytes().to_vec())` coerce가 String → CharArray 변환 처리.
- **`struct link::flags` 부호 있는 비트 필드 UB** (`e88a186`, medium) — ⚠️ **N/A (eliminated)**: Rust에 C bitfield 자체가 부재. `LinkProcessPolicy` / `MonitorSwitch` enum + 별도 boolean 필드.
- **메뉴 필드 변환: 범위 초과 enum 인덱스에 대해 숫자 문자열 반환** (`b460c26`, partial) — ⏭️ **ALREADY**: `mbbi/mbbo` 등 string lookup이 valid 인덱스면 ZRST/ONST/...을 반환, out-of-range면 `format!("{}", val)` fallback.

---

### 9-C. `base-rs` — 흐름 제어 (Flow-Control)

> 본 세션 일괄: **⏸️ DEFERRED**.

- **`logClient`: 연결 끊김 시 미전송 버퍼 버리지 않기** (`0a3427c`, medium) — ⚠️ **N/A (design diff)** — 섹션 7-C 항목과 동일 (logClient TCP forwarder 미구현).
- **필터가 DB 링크 읽기 경로(`dbDbGetValue`)에 적용되지 않음** (`17a8dbc`, medium) — ✅ **DONE** (this session): `FilterChain::apply_to_read_value(value)` 헬퍼 추가 — 단일 value를 synthetic FilteredMonitorEvent (mask=VALUE) 로 wrap, 체인 적용, 결과 value 반환. Stream-only 필터 (dbnd/dec/sync) 는 single-read 컨텍스트에서 의미 미정의지만 framework는 chain spec 그대로 실행 (운영자 책임). `arr`/`ts` 같은 single-read-meaningful 필터는 자연스럽게 동작.
- **DB 링크가 `dbChannel` 대신 `DBADDR`를 저장하여 필터 메타데이터 손실** (`b1f4459`, medium) — ⚠️ **N/A (design diff)**: 섹션 7-C 동일 항목 참조 — `ParsedLink::Db(DbLink { record, field, policy, monitor_switch })` 가 DBADDR/dbChannel 분리 자체를 갖지 않음. 필터 chain이 추후 link 안에 들어오면 `DbLink` 에 Vec<FilterSpec> 필드 추가로 자연스럽게 확장.
- **`logClient` 재연결 후 미전송 메시지 즉시 플러시되지 않음** (`9df98c1`, partial) — ⚠️ **N/A (eliminated)**: epics-rs는 `tracing` crate 사용. logClient TCP forwarder 자체가 부재.

---

### 9-D. `ca-rs` — 네트워크 라우팅 (Network-Routing)

- **`rsrv`: 클라이언트 공급 호스트명 대신 검증된 IP 주소 사용** (`530eba1`, high) — ✅ **DONE (R7-16)**: C와 동일하게 `asCheckClientIP` 전역 플래그로 구현. 기본값 0 = C 기본값(클라이언트가 보낸 호스트명을 그대로 신뢰, HAG는 이름으로 매칭), `asCheckClientIP 1` = peer IP를 권위로 사용하고 ACF 로드 시 HAG 항목을 IP로 해석. 이전 기록의 `EPICS_CAS_USE_HOST_NAMES`는 epics-base에 존재하지 않는 변수였고, 그 "기본값이 C와 같다"는 서술도 틀렸음.
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

- **`dbPut` long-string(nRequest>1) 경로에서 `get_array_info` 스킵** (`82ec539`, medium) — ⚠️ **N/A (design diff)**: epics-rs는 long string을 `EpicsValue::String` 으로 단일 표현, 별도 `nRequest` parameter 없음. CA wire의 long string handling은 codec layer에서 길이-prefix 명시적 처리 — get_array_info skip 패턴 부재.
- **`db_field_log` DBE 마스크 누락으로 필터가 `DBE_PROPERTY` 구분 불가** (`235f8ed`, medium) — ⚠️ **N/A (design diff)**: 섹션 7-A 동일 항목 — epics-rs filter chain이 받는 `FilteredMonitorEvent::posting_mask: EventMask` 가 시작부터 mask 정보 보유. db_field_log struct 자체가 부재.
- **`caput`으로 0원소 배열 쓰기 허용** (`a42197f`, medium) — ⚠️ **N/A (semantic diff)**: 12cfd41 가드와 동일 — `field_io.rs:88-92` 가 빈 배열을 스칼라 필드로 coerce 시 명시적 reject. 빈 배열을 array 필드에 쓰는 것은 unsupported (현재 사용 사례 없음).
- **CA count=0이 가변 크기 배열 구독을 의미함을 문서화** (`8c99340`, low) — ⏭️ **ALREADY**: CA 서버측 EVENT_ADD 핸들러가 count=0를 "send full array length on every monitor" semantic으로 처리. 명시적 docs 추가 필요 없음 (기존 행동).

---

### 9-F. `base-rs` / `ca-rs` — 기타 (Other)

> 본 세션 일괄: **⏸️ DEFERRED**.

- **`recGblRecordError`: 음수 상태 코드에 대한 오류 심볼 조회 건너뜀** (`4c20518`, medium) — ⚠️ **N/A (eliminated)**: epics-rs는 errSymbol 테이블 미사용; `CaError` enum 이 음수/양수 구분 없이 모든 variant가 `Display` 구현 — 메시지 누락 자체가 부재.
- **`iocsh` 인자 분리기: EOF 센티널 (-1)이 유효 문자로 처리** (`3dbc9ea`, partial): `iocsh` 파서에서 -1이 EOF가 아닌 정수로 처리되는 버그. → `iocsh/mod.rs`
- **`aSub` 레코드: 상수 입력 링크에 `dbGetLink` 호출 오류** (`d47fa4c`, partial) — ⏭️ **ALREADY**: 섹션 5/7-C 보강 참조 (multi-input fetch가 `ParsedLink::Constant`를 별도 분기로 처리).
- **`subRecord`: 잘못된 `INP` 링크 오류를 조용히 성공으로 처리** (`832abbd`, partial) — ⏭️ **ALREADY** (PR #4737901 cycle): `processing.rs::process_record_with_links_inner` 가 soft-channel INP read 가 None 반환하고 INP가 실제 Db/Ca/Pva 링크였을 때 `LINK_ALARM/INVALID` 자동 부착 (database_tests::test_soft_inp_read_failure_sets_link_alarm).
- **`iocsh`에 `iocshSetError`로 오류 코드 전파** (`144f975`, partial): → 섹션 7-C 기존 항목 보강.
- **`waveform` `NORD`가 타임스탬프 갱신 전에 발송 → 첫 CA 모니터에 미정의 타임스탬프** (`5ba8080`, medium) — ⏭️ **ALREADY** (cycle 1, PR #359): NORD post가 snapshot path를 통해 `apply_timestamp` 후 발송. 회귀 테스트 `test_array_records_nord_monitor_uses_post_process_timestamp` 4 ArrayKind 모두 cover.

---

### 9-G. new-notes PR (최신 미병합 기능)

`documentation/new-notes/`에서 발굴한 현재 개발 중이거나 병합 예정인 기능들:

- **PR #359: `aai`/`aao`/`subArray`/`waveform`의 `NORD` 필드 타임스탬프 버그 수정** — ✅ **DONE**: 레코드 타입 `a02c310`로 구현 + NORD 타임스탬프 순서 수정은 구조적으로 enforced. Rust 측 모든 notify 경로(`process_record_with_links_inner`, `AsyncPendingNotify`, `complete_async_record_inner`)가 `apply_timestamp` → snapshot 빌드 → `notify_from_snapshot` 순서로 호출되므로 NORD 이벤트는 항상 post-process timestamp를 carries. 회귀 테스트 `test_array_records_nord_monitor_uses_post_process_timestamp` (`crates/epics-base-rs/tests/database_tests.rs`)이 4 종류(Waveform/Aai/Aao/SubArray) 모두 검증.
- **PR #768: `iocInit`에서 로컬 CA 링크 연결 대기** — ✅ **DONE**: `IocApplication::run`이 `setup_cp_links` 직후 `PvDatabase::wait_for_external_links(timeout)`를 호출해 모든 등록된 LinkSet(dbCa/dbPv 양쪽)의 외부 링크가 connect될 때까지 대기. timeout은 `EPICS_RS_INIT_LINK_TIMEOUT` env (기본 10초, 0이면 wait skip). C의 `initOutstanding`/`DBCA_CALLBACK_INIT_WAIT` 카운터 + 별도 hook을 LinkSet trait의 `is_connected(name)` 폴링으로 단순화 — 결과 동등. `wait_for_external_links_*` 단위 테스트 3종(no lsets / quick-connect / partial-on-timeout).
- **PR #788: `epicsThreadGetCPUs` 및 `callbackParallelThreads` CPU 어피니티 반영** — ⏭️ **ALREADY**: 섹션 3 보강 (Rust `std::thread::available_parallelism()`).
- **PR #812: `dbCreateRecord` iocsh 명령어** — ⏭️ **ALREADY**: `cmd_db_create_record` (`crates/epics-base-rs/src/server/iocsh/commands.rs:680`)로 등록 + 5종 테스트(happy path + duplicate / bad name / unknown type / missing args).
- **~~PR #817: `mbbi` 레코드의 `AFTC`/`LALM` 버그 수정~~** — ⚠️ **CORRECTED (ledger A2-R2-4):** 기존 "bi/mbbi AFTC" 전제가 틀렸다. `biRecord.c`에는 `AFTC`/`AFVL` 필드도, 알람 필터도 없다 — 경보-범위 저역통과 필터는 2009 EPICS Codeathon 작업(`824d37811`)으로 `ai`/`calc`/`longin`/`mbbi`(+이후 `int64in`)에만 존재한다. 정정된 현황: (1) 필터는 `records/alarm_filter.rs::aftc_filter`로 일원화돼 `evaluate_analog_alarm`(ai/calc/longin/int64in)과 `mbbi.rs`에서 호출된다 — bi 과잉 포팅(날조된 `AFTC`/`AFVL` 필드+필터)은 제거됨. (2) AFVL writeback은 `record.put_field("AFVL", …)`로 매 cycle 기록(C mbbi는 writeback이 없어 필터가 inert — A2-R2-3에서 keep-vs-revert 미결정). (3) mbbi/bi의 "COS 발화 시에도 LALM 항상 update"는 C의 `if (val==lalm || recGblSetSevr(COS))return;` 단축평가(`recGblSetSevr`가 severity 상승 시 TRUE 반환 → LALM update skip)와 **갈리는** Rust 동작이며 "수정된 버그"가 아니라 별도 parity 미결 사항이다. 회귀 테스트: `test_mbbi_aftc_writes_afvl_back_each_cycle`, `test_mbbi_lalm_updates_when_cosv_set`, `test_bi_lalm_updates_when_cosv_set`, ai/longin/int64in seed 테스트 3종, pure-function `aftc_filter_tests`. (날조된 bi AFTC 테스트 2종은 제거됨.)

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

- **공유 PV(SharedPV) 에러 경로 데드락 방지** (`b17f8207676d`, high) — ⚠️ **N/A (eliminated)**: pvxs `SharedPV` 의 errored-path lock release 순서 cycle 문제. epics-rs에는 pvxs SharedPV 등가 구조가 없음 — `Arc<RwLock<RecordInstance>>` 단일 잠금 모델로 lock 순서 cycle 가능성 부재.
- **잘못된 SID(Session ID) 처리** (`280919b3ec08`, medium) — ⏭️ **ALREADY**: `server_native/tcp.rs::handle_*` 가 모두 `channels.get(&sid).match { Some(ch) => …, None => … }` 패턴으로 잘못된 SID를 silently 처리. CMD_DESTROY_CHANNEL의 `channels.remove(&sid)` 도 멱등 (이미 없으면 no-op).
- **채널 누수(Channel leak) 차단** (`289f508af6fe`, medium) — ⏭️ **ALREADY**: tokio task가 drop되면 `Drop` impl로 모든 channel 상태 정리. 클라이언트 disconnect (TCP read=0 또는 IO error) 시 `handle_client` 가 task 종료, spawn된 모든 sub-task가 mpsc 닫힘으로 cascade 종료.
- **초기 ACK 없는 Monitor 처리** (`2f4484889186`, medium) — ⏭️ **ALREADY**: epics-pva-rs monitor 는 `mpsc::channel(64)` + `coalesced: Mutex<Option>` 패턴 (`server_native/tcp.rs:587`). client ACK 등가는 mpsc capacity로, ACK 없으면 try_send fail → coalesced에 last-value 보관 — 누락 자체가 부재.
- **GET_FIELD 마지막 연결 끊김 처리** (`5019744fa79c`, medium) — ⏭️ **ALREADY**: tokio future가 await에서 disconnect 감지, IO error 반환 → `handle_client` 가 정상 종료. C의 manual cleanup 등가 작업이 RAII로 자동.
- **`autoExec=false` PUT 중 원격 오류 처리** (`70735383350b`, medium) — ✅ **DONE** (this session): `OpState` 에 `put_auto_exec: bool` + `put_pending: Option<PvField>` 필드 추가. INIT pvRequest 의 `record._options.autoExec` 문자열 ("false"/"no"/"0" → false, "true"/"yes"/"1" → true) 파싱 (`put_autoexec_from_request` 헬퍼 + 7 단위 테스트). PUT execute branch 분기: autoExec=true는 즉시 실행 (기존 동작); autoExec=false 첫 번째 PUT는 값 queue + OK ack; 두 번째 (commit) PUT는 queued 값을 source.put_value_checked 로 실제 쓰기. 원격 오류는 동일한 status path 로 전파.
- **TX 버퍼 한계를 확인하여 스로틀링** (`8d58409481ef`, medium) — ⏭️ **ALREADY**: `tcp.rs:587-625`에 dedicated writer task + mpsc backpressure + per-monitor coalesced slot 구조. mpsc full 시 `try_send` 실패하면 `coalesced` 에 last-value 저장 — 메모리 무한 증가 차단.

### 10-C. `pva-rs` — 와이어 프로토콜 & 디코딩 (Protocol & Decoding)

- **`SetEndian` 제어 메시지 올바른 처리** (`cce797263d1d`, high) — ⏭️ **ALREADY**: `proto/command.rs::ControlCommand::SetByteOrder`가 정의되어 있고 `server_native/tcp.rs:574`에서 handshake에 emit + 클라이언트 측에서도 수신 처리.
- **배열(Array) 디코드 버그 수정** (`cf91bc3033e2`, high) — ⏭️ **ALREADY**: epics-pva-rs `pvdata::decode` 가 size prefix를 `decode_size`로 명시적 파싱 후 `Vec::with_capacity(n)` + element-by-element decode. C의 `vector.resize()` + raw memcpy 크래시 패턴이 부재 — Rust slice/Vec API가 모든 boundary check를 강제.
- **디코드 오류 시 원격 `file:line` 정보 추출** (`e9ce80880d92`, high) — ✅ **DONE** (this session): `Status::error_with_location(file, line, msg)` 가 stack-trace 필드에 `<file>:<line>` 포맷으로 인코딩, `Status::source_location()` 가 round-trip 파싱. 회귀 테스트 4종 (round-trip / OK 시 None / Windows 경로 (콜론 포함) / malformed stack rejection).
- **`null` 문자열 디코딩** (`0356eee74037`, medium) — ⏭️ **ALREADY**: epics-pva-rs `pvdata::decode::decode_string` 이 length=0 또는 length=-1 (varint sentinel) 모두 빈 String으로 normalise. `Option<String>` 변환은 위 레이어 (PvField → 사용자 type)에서 처리.
- **`CMD_MESSAGE` 처리 수정** (`0eea8fd1c7e0`, medium) — ⏭️ **ALREADY**: `proto/command.rs::Command::Message` 가 디코드 + display + handler 모두 정의 (`server_native/tcp.rs::handle_message_command`).
- **자격 증명(Credentials) 디코드** (`7de1f7d32f63`, medium) — ⏭️ **ALREADY**: PVA Connection Request 의 `auth` 필드 디코드는 `proto/auth.rs::ConnectionAuth` (`anonymous`/`ca`/`x509` 3가지 method). x509 mTLS 경우 peer cert에서 issuer DN 추출 (`tls::issuer_from_cert`).

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
