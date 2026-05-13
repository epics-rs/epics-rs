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
- **IPv6 지원 (PR #205)** — ⏸️ **DEFERRED**: `epics-ca-rs`의 `ADDR_LIST` 및 `BEACON` 멀티캐스트 로직이 아직 IPv4 전용. 62 Ipv4Addr sites/8 files, CA wire 4-byte IP 필드 구조 제약, dual-stack + IPv6 multicast semantics, AsyncUdpV4 generic화 필요 (~2000 LOC, multi-day PR).
- **DNS 변경 시 영구 연결 끊김 현상 (Issue #488)** — ⏭️ **ALREADY**: round-50 작업으로 `EPICS_CA_DNS_REFRESH_SECS` + `AddrEntry::refresh_dns` 구현됨 (`crates/epics-ca-rs/src/client/search.rs:354`).
- **TLS 기반 보안 pvAccess (PR #641)** — ✅ **DONE** `23360e6`: `tls::issuer_from_cert` 추가, `ClientState{auth_method, auth_authority}` 필드, `compute_access`를 `check_access_method`로 전환. mTLS peer cert의 issuer DN이 `AUTHORITY()` ACF 절과 매칭되며 method는 `"x509"` 고정.
- **CA 클라이언트의 서버 프로토콜 버전 결정 (PR #711)** — ⏭️ **ALREADY**: `transport.rs`가 CA_PROTO_VERSION 수신 시 `server_minor_version`을 캡처하고 `send_echo`에서 v4.3+ ECHO vs 이전 READ_SYNC로 분기.
- **지정된 TCP 포트 + UDP 5064 분리 (PR #69)** — ✅ **DONE** `9d8a34b`: `cas_server_port()`, `CaServerBuilder::tcp_port`, `IocApplication::tcp_port`. UDP 응답기가 실제 바인딩된 TCP 포트를 SEARCH_REPLY에 광고.
- **절전 모드(Suspend) 해제 후 CA 멈춤 현상 (Issue #190)** — ✅ **DONE** `a409311`: wall-clock skip 기반 suspend wake 탐지, echo probe 5s→1s 단축, tracing::info 기록. 절전 후 복구 ~1s.
- **서버 측 채널 필터 (Server-side Filters, 3.15.7)** — ⏸️ **DEFERRED**: PV-name JSON 파서 + SubscriptionFilter trait + 모든 monitor 발행 경로 통과 + 4개 필터 타입(decimation/arr/ts/sync) ~1000 LOC + 별도 design review 필요.

---

## 2. IOC, 레코드 및 데이터베이스 (Records & Database)
- **`aai`, `aao`, `subArray` 등 배열 레코드 부재 (PR #162, #742)** — ✅ **DONE** `a02c310`: `ArrayKind` 열거형으로 `WaveformRecord` 공유, `aao`만 `can_device_write=true`. NORD 이벤트는 기존 waveform 경로에서 처리. 단, `subArray`의 INDX/MALM 슬라이싱 시맨틱은 후속 작업.
- **`dbServerStats()` API 구현 지연 (PR #592)** — 🔄 **PARTIAL** `ac92e3e`: `ServerStats`에 channels_opened/closed + subscriptions_opened/closed + bytes_in/out 카운터 추가. channel 카운터는 `ServerConnectionEvent`로 wired; subscription/bytes counter는 declared (transport-level wiring deferred).
- **`dbLoadTemplate`의 `EPICS_DB_INCLUDE_PATH` 지원 (PR #636)** — ⏭️ **ALREADY**: `iocsh/commands.rs:876`이 env var를 읽어 include path 리스트 구성.
- **잘못된 필드명에 대한 자동 제안 (PR #689)** — ⏭️ **ALREADY**: round 23 commit으로 `dbpf` typo suggestion 구현.
- **`dbPut` / `dbGet`의 16진수 및 8진수 문자열 지원 (PR #678)** — ✅ **DONE** `17210b4`: 기존 `parse_int`가 정수 타입 prefix를 처리했고, 본 commit으로 Double/Float도 `parse_string_to_f64`로 통합 (sign + 0x hex + 0-leading octal + 일반 decimal/exponent).
- **`asTrap` 내 `dbChannel` 노출 (PR #501)** — ⚠️ **N/A**: epics-rs는 generic asTrap 인터페이스 대신 `epics_ca_rs::audit::AuditLogger`를 사용하며, PV 이름·user·host·method·deny-reason 등 dbChannel-equivalent 컨텍스트를 이미 JSON으로 기록.
- **레코드 삭제 기능 (PR #505)** — ⏭️ **ALREADY**: `iocsh/commands.rs:24`에 `cmd_db_delete_record` 등록되어 있음.
- **`getenv` 디바이스 지원 (3.15.4)** — ✅ **DONE** `7ed3baf`: 신규 모듈 `server/builtin_devices/getenv.rs`. `IocBuilder::new()`/`IocApplication::new()`에서 `DTYP="getenv"`로 자동 등록. INP의 `@` prefix 처리.
- **출력 레코드의 `SIMM=RAW` 시뮬레이션 모드 (7.0.7)** — 🔄 **PARTIAL** `ac92e3e`: SIMM=2 명시 인식, RVAL-있는 레코드(ai/ao)에서 raw value path. RVAL 없는 레코드는 SIMM=YES와 동일 fallback.
- **`longout` 레코드의 조건부 출력 `OOPT` 필드 (7.0.8)** — ✅ **DONE** `73b517c`: EpicsRecord derive를 manual `impl Record`로 교체, `should_output` override + 신규 trait method `on_output_complete` 추가, processing.rs의 device-write/soft-link 양 경로에 OOPT gate.

---

## 3. Shell (iocsh) 및 런타임 환경 (Runtime/Environment)
- **비대화형(non-interactive) `readline` 스킵 (PR #848)** — ✅ **DONE**: `IocShell::run_repl`이 `std::io::stdin().is_terminal()` 으로 분기 — TTY에서는 기존 rustyline 인터랙티브 경로, non-TTY(파이프/here-doc/`<script.cmd`)에서는 `run_repl_piped`가 `BufRead::lines()` 로 단순 읽기 + 프롬프트 출력 skip. 백그라운드 실행 시 captured stderr에 `epics>` 노이즈가 더 이상 섞이지 않음.
- **`iocshLoad` 명령어 미지원 (Issue #847)** — ✅ **DONE**: `IocShell::execute_line`이 `iocshLoad <path> [macros]` (space + C++ `iocshLoad("path","K=V,...")` paren form 양쪽 지원)을 인터셉트. `execute_script_with_macros`가 `db_loader::substitute_macros`로 라인별 `$(KEY)`/`${KEY}` 치환 후 재귀적으로 `execute_line` 디스패치. 빈 macros일 때는 substitution skip. 라인별 에러는 `execute_script`와 동일하게 다음 라인 진행 + 최종 Err 반환 (`iocshSetError` 등가). 테스트 5종: space form macro, paren form, no-macros, missing-path-error, per-line-error-propagate.
- **가용 CPU 수치 과다 보고 방지 API (PR #788)** — ⏸️ **DEFERRED**: `taskset` 등 어피니티 제한 환경에서 실제 가용 CPU 수 보고 미구현.
- **`SIGTERM` / `SIGINT` 수신 시 `atExit` 정상 종료 절차 (PR #671)** — 🔄 **PARTIAL**: CA 서버(`ca_server.rs`)·PVA 서버 runtime·`epics-tools-rs::procserv`에는 SIGTERM 핸들러 존재. `epics-base-rs::server::ioc_app`에는 명시 핸들러 없음(드롭 시 자연 종료에 의존).
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
- **네임서버 CA 프로토콜 강제 지정 (PR #621)** — ⏸️ **DEFERRED**: `CA_V413` 강제 옵션 미구현.
- **다량 채널 검색(Mass-channel) 성능 튜닝 (Issue #372)** — 🔄 **PARTIAL**: AIMD search budget + 30-bucket cooperative tick 기반 구현되어 있으나 mass scenario 별도 검증 필요.
- **`caget` 반환 타입 단축 (PR #629)** — ⚠️ **N/A**: 원PR은 C `caget`의 `-d` argument parser에서 `DBR_INT` 문자열을 `DBR_SHORT`로 치환하는 패치(`dbr_text_to_type`이 `INT` suffix를 파싱하지 못하므로). `caget-rs`의 `-d/--dbr-type` 옵션은 현재 parity-only로 받기만 하고 채널 요청에 반영되지 않으므로(`bin/caget-rs.rs:233-235`) 변환 자체가 적용될 진입점 없음. `-d`를 실제로 wire-through할 때 같이 처리.
- **CA UDP 전송 오류 rate-limit (cae597d, c23012d)** — ⏭️ **ALREADY**: `client/search.rs::send_with_fanout`, `server/beacon.rs::run_beacon_emitter`에 per-destination first/change/recovery만 로그하는 dedup.
- **EPICS_CA_MCAST_TTL (3.16 f2a1834d)** — ✅ **DONE** `ae277d1`: `runtime::net::ca_mcast_ttl` + `AsyncUdpV4::set_multicast_ttl_v4` + CA 서버 beacon/UDP 응답기·클라이언트 search 소켓에 적용.
- **EPICS_IOC_IGNORE_SERVERS (6efe2924)** — ✅ **DONE** `8615bb4`: ADDR_LIST 파싱·SEARCH 응답·beacon 수신 3개 경로에서 quarantine IP 필터.
- **asLib DNS soft fallback (libcom 932e9f3)** — ✅ **DONE** `6862ef0`: ACF HAG 파싱 시 DNS 실패해도 abort 대신 literal 유지 + 가능한 IP 추가.

### 레코드 & 데이터베이스 세부

> 이 하위 섹션 항목 대부분은 점검·검증 작업이 본문 아래에서 별도로 다뤄집니다. 본 세션에서는 직접 다루지 않음 → **⏸️ DEFERRED**.

- **범용 `TOUT` (Timeout) 레코드 필드 부재 (PR #803)** — ⏸️ DEFERRED.
- **Soft Time Part 디바이스 지원 (PR #776)** — ⏸️ DEFERRED.
- **`bi` / `bo` 변환(Conversion) 로직 누락 (PR #775)** — ⏸️ DEFERRED.
- **상수 링크(Constant Link)의 오프셋 계산 버그 대조 (PR #467)** — ⏸️ DEFERRED.
- **사용되지 않는 `INPx` 링크 파손 시 `calc` 레코드 중단 문제 (Issue #823)** — ⏸️ DEFERRED.
- **`mbboDirect`의 `B0..BF` 필드 ASL0 권한 조정 (PR #439)** — ⏸️ DEFERRED.
- **`dbLoadRecords` 매크로 기본값 의미론 불일치 (PR #463)** — ⏸️ DEFERRED.
- **DB 파서의 알 수 없는 필드명 힌트 제공 (PR #434)** — ⏭️ ALREADY: round 23의 dbpf typo suggestion이 부분적으로 동일 기능 제공.
- **aSub 레코드의 상수 `INP*` 허용 여부 (Issue #284)** — ⏸️ DEFERRED.
- **긴 문자열 `CALC$` 지원 이슈 (Issue #194)** — ⏸️ DEFERRED.
- **`DBF_MENU` → `DBF_STRING` 변환 버그 픽스 대조 (Issue #183)** — ⏸️ DEFERRED.
- **`zero-length` (길이가 0인) 배열 지원 엣지 케이스 (7.0.5)** — ⏸️ DEFERRED.
- **`compress` 레코드 개선 (7.0.8)** — ⏸️ DEFERRED.

### Shell & 시스템 코어 세부

> 본 세션에서 직접 다루지 않음 → **⏸️ DEFERRED**.

- **`iocsh` 다중 후행 줄바꿈(trailing newlines) 트리밍 (PR #371)** — ⏭️ **ALREADY**: Rust `String::trim()`이 연속 줄바꿈 포함 모든 leading/trailing whitespace를 제거. `iocsh/mod.rs:38/159/229/231` 모두 명령 라인을 `trim()` 후 처리.
- **`initHookRegister` 멱등성 보장 로직 (PR #594 / 13d6ca5)** — ⚠️ **N/A (design diff)**: Rust 측은 builder pattern으로 per-`IocApplication`의 `after_init_hooks: Vec<Box<dyn FnOnce>>` (`crates/epics-base-rs/src/server/ioc_app.rs:68`)만 존재. C의 전역 `functionList` linked list와 달리 module-static-init/iocsh 양쪽에서 동일 fn pointer가 누적되는 시나리오가 구조적으로 발생하지 않음. 보너스: closure는 비교 불가능하므로 dedup도 의미 없음 (사용자가 의도적으로 두 번 등록한 경우 그대로 두 번 실행).
- **새로운 문자열 유틸리티 부재 (7.0.5/7.0.6)** — ⏸️ DEFERRED: `epicsStrSimilarity()`, `epicsStrnGlobMatch()` 러스트 매핑 없음.
- **빈 인스턴스의 `dbLoadTemplate` vs `msi` 파서 불일치 (Issue #666)** — ⏸️ DEFERRED.
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
- **`DBE_PROPERTY` 이벤트 중복 발송 방지**: 필드 값이 실제로 바뀐 경우에만 `DBE_PROPERTY` 이벤트를 발송하도록 수정 (`faac1df1`, 2024). `epics-rs`의 DBE 이벤트 마스크 로직 교차 검증 필요.
- **`DBE_PROPERTY` → `DBE_VALUE` 순서 보장**: `DBE_PROPERTY`를 반드시 `DBE_VALUE`보다 먼저 전송해야 하는 순서 의무 (`b7cc33c3`, 2024). `epics-rs` 구독 이벤트 전송 순서 확인 필요.
- **`mbbi`/`mbbo`의 `DBE_PROPERTY` 누락 버그**: 해당 레코드에서 `DBE_PROPERTY`가 아예 발생하지 않는 C++ 버그. `epics-rs` 구현에서 동일한 패턴 여부 점검.
- **빈 배열(length=0) `caput` 시 DBR 오프셋 오계산** (`8cc20393`, 2020): DBR 헤더에서 첫 번째 원소의 크기를 잘못 계산. `epics-rs`의 `dbr::encode_array` 엣지 케이스 확인.
- **빈 배열 `caput` 시 스칼라에 `INVALID_ALARM` 설정** (`12cfd418`, 2020) — ⏭️ **ALREADY** (semantic diff): `put_pv` (`crates/epics-base-rs/src/server/database/field_io.rs:83-92`)가 commit 12cfd41 hash까지 명시한 가드로 `value.is_empty_array() && target_is_scalar` 케이스를 `CaError::InvalidValue` 반환으로 reject. C의 LINK_ALARM/INVALID_ALARM 세트 대신 Err 전파로 fail-fast — converter는 호출되지 않으며 garbage value 작성도 차단됨. 알람 필드 변경은 발생하지 않으므로 stat/sevr monitor 채널은 영향 없음 (C와 미세 차이).
- **`dbGet`으로 빈 배열을 스칼라로 읽을 때 크래시** (`39c8d561`, 2020): 배열 원소가 0개일 때 스칼라 `dbGet` 경로가 크래시하는 버그.
- **`UTAG` uint64 타입 필드 전파** (`b94afaa0`, 2020) — ⚠️ **N/A** (의도적): `epics-rs`의 `snapshot.user_tag: i32`는 PVA Normative `time_t.userTag = int` 스펙과 일치하는 wire-correct 표현. upstream의 internal uint64는 CA-level UTAG 노출 + db_field_log 전파(둘 다 미구현)에 의미. CA-level UTAG가 필요해질 때 i64로 승격 + PVA encode에서 truncate-with-warning.
- **`amsg`/`utag`의 `dbGet()` 옵션 통로 분리** (`bd3ecf1c`, 2021): 알람 메시지(`AMSG`) 및 `UTAG`를 `dbGet()` 옵션 경로로 별도 분리. 두 필드의 `epics-rs` 직렬화 경로 교차 검증 필요.
- **`db_field_log::mask` 필드** (`235f8ed2`, 2020): 모니터 페이로드의 이벤트 마스크가 실제 발행 마스크로 덮어쓰기 되어야 함.
- **CA 서버 프로토콜 버전 클라이언트 노출** (`d7635413`, 2025, PR #711) — ⏭️ **ALREADY**: 섹션 1 참조 (transport.rs server_minor_version 분기).
- **`SOCK_CLOEXEC` 사용 + `accept4()`** (`cf3173b6`, 2021) — ⏭️ **ALREADY**: Tokio가 내부에서 처리, `epics-rs` 직접 점검 불필요.
- **`IPPORT_USERRESERVED` 포트 상수 정의** (`cd0e6a4f`/`0cae0db`, 2020-2021) — ⚠️ **N/A (eliminated)**: musl libc 헤더 호환 shim. Rust `std::net`/tokio는 `IPPORT_USERRESERVED`를 사용하지 않으며 epics-ca-rs는 하드코딩 상수/환경변수로 포트 선택. `rust_verdict: eliminated`.
- **`16진수/8진수 문자열 dbPut/dbGet 지원`** (`88bfd6f3`, 2025, PR #678) — ✅ **DONE** `17210b4` (섹션 2 항목과 동일).
- **`bi` 레코드 소프트 채널에서 `MASK` 비트 사용** (`f2fe9d12`, 2023) — ⏸️ DEFERRED.

---

### 7-B. 레코드/DB 경계값 및 배열 (Bounds, 56건 → 핵심 점검 항목)
- **`constant link` 오프셋 오프바이원(off-by-one) 버그** (`1b460770`, 2024): → 섹션 5의 기존 항목과 동일.
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
- **`casStatsFetch()` RSRV 미초기화 시 안전성** (`7a6e11ca`, 2026): RSRV가 초기화되기 전에 CA 서버 통계를 조회하면 크래시하는 버그. `epics-rs`의 `dbServerStats()` 초기화 보호 확인.
- **`dbGet`의 루프-안전 래퍼** (`dac620a7`, 2024): `dbGet()` 재귀 호출 시 데드락을 방지하는 루프-안전 래퍼 추가.
- **`NAMSG` 알람 문자열 필드를 `NSTAT`/`NSEV`와 함께 초기화** (`8483ff95`, 2024) — ⏭️ **ALREADY**: `rec_gbl_reset_alarms`(`crates/epics-base-rs/src/server/recgbl.rs:121`)가 `common.amsg = std::mem::take(&mut common.namsg)`로 promote 직후 namsg를 자동 클리어. `reset_alarms_transfers_amsg_and_clears_namsg` 테스트로 회귀 방어.
- **`lset::getAlarmMsg()` API** (`5143c71a`, 2020): 링크 세트(link set)에서 알람 메시지를 직접 읽어오는 새 API.
- **빈 문자열 링크를 `unset`과 동일하게 처리** (`3b484f58`, 2023): `INP`/`OUT` 링크가 빈 문자열 `""`일 때 링크를 해제(unset)된 것으로 처리해야 하는 시맨틱.
- **`FIFO 스케줄링`을 환경 변수로 비활성화** (`862272d6`, 2025): `EPICS_NO_RT_SCHED` 같은 환경 변수로 RT 스케줄링을 비활성화하는 기능. `epics-rs`의 RT 스레드 옵트아웃 로직 확인.
- **`memlock()` 옵트아웃** (`0916cf98`, 2025): FIFO 스케줄링이 비활성화된 경우 `mlockall()` 호출도 건너뜁니다.
- **`aSub` 레코드의 상수 `INP*` 링크 지원** (`d47fa4ca`, 2022, Issue #284): → 섹션 5의 기존 항목과 동일.
- **`dbLoadRecords()` 오류 메시지 중복 출력 방지** (`9af7fb3`, 2025): 로딩 실패 시 에러 메시지가 두 번 출력되는 버그.
- **`dbReadDatabaseFP()` 파일 닫기 보장** (`a6779df2`, 2022) — ⚠️ **N/A (eliminated)**: Rust `std::fs::File`의 `Drop`이 자동으로 `close()` 보장. `BufReader<File>` 등 모든 파일 래퍼 동일. `rust_verdict: eliminated`.
- **`logClient` 연결 끊김 시 미전송 메시지 재전송 시도** (`0a3427c8`, 2019): 로그 서버와의 연결이 끊어져도 버퍼에 남은 메시지를 바로 버리지 않고 재전송 시도. `epics-rs`의 `logClient` 셧다운 경로 확인.
- **알람 메시지 필드(`AMSG`) 및 타임 태그 필드(`UTAG`) 추가** (`892a361d`/`b94afaa0`, 2020): `recGbl`에 알람 문자열 필드(`AMSG`)와 64비트 사용자 태그(`UTAG`)가 추가됨. `epics-rs` 레코드 공통 필드 동기화 필요.
- **`dbChannel` 기반 링크 (DBADDR → dbChannel 교체)** (`b1f44592`, 2020): 내부 링크의 주소 타입이 `DBADDR`에서 `dbChannel`로 교체되는 대규모 리팩토링. `epics-rs`의 링크 어드레싱 모델 확인.

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
- **`callbackParallelThreads` 비율(%) 지정 지원** (`fe39a007`, 2026): 콜백 스레드 수를 CPU 코어 수의 백분율로 지정하는 기능. `epics-rs`의 Tokio 런타임 워커 설정에 준하는 노출 방식 검토.
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
- **`EPICS_TZ` 환경 변수로 표준화** (`b0db6568`, 2019): `EPICS_TIMEZONE`을 대체하는 POSIX 표준 `EPICS_TZ` 변수 지원.
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

- **`waveform` 레코드 `PACT=TRUE` 유실** (`16c3202`, high): 비동기 완료 시 `PACT` 플래그가 소실되어 이중 처리(double-processing)가 발생. → `waveform_record.rs`
- **`errlog` 이중 버퍼링 재작성** (`29fa062`, high): 출력 중 락(Lock)을 보유하는 구조를 이중 버퍼링으로 교체. → `errlog.rs`
- **`scanStop()` 전 `scanStart()` 누락 시 크래시/행(Hang)** (`0a6b9e4`, high): 초기화 전 정지를 호출할 때의 보호 로직. → `scan.rs`
- **`db_field_log`: `dbfl_type_rec` 제거, `dbfl_type_ref` 통합** (`27fe3e4`, high): 필드 로그의 라이브 레코드 참조 방식 통합. → `db_field_log.rs`
- **`dbGet`: `db_field_log` vs 라이브 레코드 선택 조건 버그** (`56f05d7`, high): 캐시 데이터와 라이브 데이터를 잘못 선택하는 버그. → `db_access.rs`
- **`dbDbLink processTarget` 자기-링크 RPRO 무한 루프** (`62c11c2`, high): 레코드가 자기 자신을 링크할 때 `RPRO` 플래그가 무한 재처리를 유발. → `dbDbLink.rs`
- **`dbPutFieldLink`: `dbChannelOpen()` 오류 상태 전파** (`8a0fc03`, high): 채널 오픈 실패 시 오류를 전파하지 않고 무시하는 버그. → `db_access.rs`
- **`db_field_log`: 데이터 소유권 추상화 누락** (`85822f3`, high): 소유권 없이 데이터에 접근하여 스캔 잠금 레이스 유발. → `db_access.rs`
- **`callbackRequest`: 미초기화 콜백 큐 접근** (`ac6eb5e`, high): iocInit 전 콜백 요청 시 초기화 보호 누락. → `callback.rs`
- **`PINI` 크래시: 힙 Use-after-free 방지를 위한 스택 필드-로그** (`e0dfb6c`, high): PINI 처리 중 필터 체인에서 힙 UAF 발생. → `links.rs`
- **`dbEvent` 안전한 종료 세마포어 셧다운 프로토콜** (`b35064d`, high): 이벤트 워커 스레드의 안전한 종료 절차. → `db_event.rs`
- **`dbEvent` 다중 `db_event_cancel()` 호출 안전성** (`fab8fd7`, high): 취소 함수를 여러 번 호출해도 안전하도록 보호. → `db_event.rs`
- **`asCaStop()` 스레드 join 데드락 방지** (`bac8851`, high): ACF CA 스레드 정지 시 데드락 방지 로직. → `as_ca.rs`
- **`iocInit` 로컬 CA 링크 연결 대기** (`717d69e`, high): `PINI` 처리 전에 로컬 CA 링크가 연결될 때까지 대기. → `ca_link.rs`
- **`longout` `OOPT=On Change` 첫 처리 시 출력 누락** (`6c573b4`, medium): 초기 처리 시 `OOPT` 로직이 변화 없음으로 판단해 출력을 건너뜀. → `longout` 레코드
- **`longout special()`: 링크 변경 플래그를 OUT 링크 갱신 전에 설정** (`1d85bc7`, medium): 잘못된 순서로 플래그를 설정하여 오래된 링크에 잘못된 이벤트를 발송. → `longout`
- **`mbboDirect`: 초기화 우선순위 버그 — `B0..B1F` bits가 `VAL`보다 우선** (`dabcf89`, medium): UDF 상태에서 VAL 대신 비트 필드로 초기화되는 문제.
- **`aai`/`waveform` 레코드 `NORD` db_post_events 정리** (`23d9176`/`5d1f572`/`aff7463`, medium): `NORD` 이벤트를 디바이스 지원 레이어가 아닌 레코드 지원 레이어에서만 발송. → `aai`, `waveform`
- **`subArray` `NORD` 변화 시 `db_post_events` 누락** (`51c5b8f`/`64011ba`, medium): 원소 수 변화 시 `NORD` 모니터 이벤트가 발행되지 않음.
- **`AMSG` 알람 메시지가 MSS 링크를 통해 전파되지 않음** (`d0cf47c`, medium): 알람 메시지 문자열이 링크를 타고 다운스트림 레코드로 전파되지 않는 버그.
- **타임스탬프가 출력 링크 처리 후 갱신되어 `TSEL` 스탤 타임스탬프 발생** (`f1e83b2`, medium): 출력 링크가 처리된 후에 타임스탬프가 갱신되어 다운스트림 `TSEL`이 오래된 값을 읽는 버그.
- **`dbNotify`: 첫 번째 레코드 호출에서만 `PUTF` 설정** (`3fb10b6`, medium): `dbNotify` 경로에서 `PUTF` 플래그가 중간 레코드에도 잘못 설정되는 버그.
- **`devAiSoft read_ai`: 디바이스 읽기 실패 시 오류 반환** (`4737901`, medium): 소프트 채널이 오류를 무시하고 성공을 반환하는 버그.
- **`initHookRegister` 멱등성 보장** (`13d6ca5`, medium): 동일한 훅 함수를 여러 번 등록해도 한 번만 실행되도록 보장. → 섹션 5 항목 보강.
- **`iocShutdown`에서 de-init hook 알림 추가** (`5d5e552`, partial): 셧다운 시퀀스에 `initHookAfterShutdown` 등 훅 발화 추가.
- **`errlog` 워커가 셧다운 전 버퍼를 비우지 않고 루프 종료** (`7448a8b`, partial): 종료 시 로그 버퍼가 드레인되지 않는 버그.
- **`errSymbolAdd`가 `errSymBld` 전에 실패** (`8c08c57`, medium): 에러 심볼 테이블 초기화 전에 심볼 추가를 시도하면 초기화 순서 버그 발생.
- **`ts` 필터: 오래된 `db_field_log` API 사용** (`e11f880`, partial): `dtor` 필드가 유니온 밖으로 이동한 이후에도 구버전 API를 참조하는 버그.
- **`Decimate`/`Sync` 필터가 `DBE_PROPERTY` 이벤트를 잘못 드롭** (`a74789d`, high): → 섹션 7-F의 기존 항목 보강 (master_index에 `base-rs/src/server/database/filters/decimate.rs`로 명시).
- **`dbEvent` 이벤트 큐 중복 참조 타입 이벤트 누적** (`4df48c9`, medium): 이벤트 큐에 동일 이벤트가 중복 쌓여 컴팩션되지 않는 버그.
- **`compressRecord`: `RES` 필드로 리셋 시 모니터 이벤트 미발송** (`8ac2c87`, medium): 리셋 필드를 통해 레코드를 초기화해도 CA 모니터가 통보받지 못하는 버그.

---

### 9-B. `base-rs` — 경계 및 타입 (Bounds / Type-system)

> 본 세션 일괄: **⏸️ DEFERRED**.

- **`histogramRecord` wdog 콜백이 `VAL` 대신 `bptr`로 이벤트 발송** (`4a0f488`, medium): 히스토그램 레코드의 잘못된 포인터로 모니터 이벤트 발송.
- **영(zero)원소 배열 읽기에 대한 고유 오류 코드** (`5d808b7`, medium): `S_db_emptyArray` 등 배열 길이 0 상황에 대한 전용 오류 코드 필요.
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

- **`logClient`: 연결 끊김 시 미전송 버퍼 버리지 않기** (`0a3427c`, medium): → 섹션 7-C의 기존 항목 보강 (파일: `errlog.rs`).
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
- **`caRepeater` 부모 프로세스의 `stdin`/`stdout`/`stderr` 상속 문제** (`6dba2ec`, partial) — ⏸️ DEFERRED.

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
- **`aSub` 레코드: 상수 입력 링크에 `dbGetLink` 호출 오류** (`d47fa4c`, partial): 상수 링크에 `dbGetLink`를 호출하면 오류 반환. → `aSub` 레코드 구현 시 주의.
- **`subRecord`: 잘못된 `INP` 링크 오류를 조용히 성공으로 처리** (`832abbd`, partial): 불량 입력 링크의 오류를 무시하는 버그.
- **`iocsh`에 `iocshSetError`로 오류 코드 전파** (`144f975`, partial): → 섹션 7-C 기존 항목 보강.
- **`waveform` `NORD`가 타임스탬프 갱신 전에 발송 → 첫 CA 모니터에 미정의 타임스탬프** (`5ba8080`, medium): `NORD` 이벤트와 타임스탬프 갱신 순서 문제. → `waveform` 레코드

---

### 9-G. new-notes PR (최신 미병합 기능)

`documentation/new-notes/`에서 발굴한 현재 개발 중이거나 병합 예정인 기능들:

- **PR #359: `aai`/`aao`/`subArray`/`waveform`의 `NORD` 필드 타임스탬프 버그 수정** — 🔄 **PARTIAL** `a02c310`: `aai`/`aao`/`subArray` 레코드 타입 자체는 신규 구현. NORD 타임스탬프 갱신 순서 fix는 별도 작업.
- **PR #768: `iocInit`에서 로컬 CA 링크 연결 대기** — ⏸️ DEFERRED: 9-A high-priority 항목 — 우선 후보.
- **PR #788: `epicsThreadGetCPUs` 및 `callbackParallelThreads` CPU 어피니티 반영** — ⏸️ DEFERRED.
- **PR #812: `dbCreateRecord` iocsh 명령어** — ⏸️ DEFERRED.
- **PR #817: `mbbi` 레코드의 `AFTC`/`LALM` 버그 수정** — ⏸️ DEFERRED.

---

## 10. Archaeology PVXS 감사 — `pva-rs` 미반영 고위험 항목

**출처**: `archaeology/pvxs/INDEX/master_index.md` (PVA/PVXS 구현체의 전체 커밋 대상 분석 결과)  
아래는 기존 PVA(pvAccess) 프로토콜 관련 미반영 항목들 중 `applies`(반영 필요) 판정을 받은 **High / Medium** 항목들입니다.

---

### 10-A. `pva-rs` — 클라이언트 & 네트워크 연결 (Client & Connection)

- **TCP Search 기능 추가** (`8363c7fe9a5f`, high) — ⏭️ **ALREADY**: `EPICS_PVA_NAME_SERVERS` 환경변수로 TCP name server 지원 (`client_native/channel.rs::new_with_name_servers`).
- **재연결 루프 지연(Slow down reconnect loop)** (`3b8540f52002`, high) — ⏭️ **ALREADY**: `channel.rs::holdoff_until` 타이머로 connect-fail holdoff 구현.
- **종료(Shutdown) 중 Name Server 재연결 금지** (`4d12da87205e`, high) — ⏸️ DEFERRED.
- **`Channel` Search Bypass 최적화** (`5d3a21f03010`, high) — ⏸️ DEFERRED.
- **`Channel` 일관된 연결 해제(Disconnect) 처리** (`f7b3821e10b4`, high) — ⏸️ DEFERRED.
- **`Context::close()` 명시적 지원** (`0de17036f4a6`, medium) — ⏭️ **ALREADY**: `PvaClient::close()` (`context.rs:610`).
- **Search 패킷 단편화(Fragmentation) 방지** (`84ef355a4a1a`, medium) — ⚠️ **N/A**: 현재 `build_search`가 count=1 단일 PV 패킷이라 MTU 미만 — batching 미구현이므로 fragmentation 발생 자체 불가.
- **환경 변수를 통한 설정 가능 타임아웃** (`da004bc54bb3`, medium) — ⏸️ DEFERRED.
- **Search 응답 처리 한계 상향** (`b38b33db034e`, medium) — ⏸️ DEFERRED.
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

- **UDP RX 버퍼 오버플로 감지** (`a064677e3625`, high) — ⏸️ DEFERRED.
- **클라이언트 비콘 수신 시작** (`acfba6469ed3`, high) — ⏭️ **ALREADY**: `client_native/search_engine.rs:598` `beacon_recv` future로 백그라운드 수신.
- **잘못된 스레드에서의 비콘 발송 경고** (`882a7720fb92`, medium) — ⚠️ **N/A**: Rust의 `Send`/`Sync` trait이 컴파일 시 보장.
- **서버 비콘 TX 최적화** (`cc5071cd22c4`, medium) — ⏸️ DEFERRED.
- **잘린 비콘(Truncated Beacon) 오류 무시** (`772cc5297cf8`, medium) / **반복적인 비콘 TX 오류 표시 제어** (`adcac746efff`, `91fed88cdd7f`) — ⏸️ DEFERRED.
- **비콘 정리 타이머 단순화** (`b33ea5df3113`, medium) — ⏸️ DEFERRED.

---

**💡 추가 요약** 
기존 C++ `epics-base`에서 발생했던 수많은 치명적인 메모리 오염, 세그폴트, NULL 포인터 역참조 및 멀티스레딩 데이터 레이스 버그들(예: PR #496, #485, #25, #745 등)은 러스트의 **메모리 안전성(Ownership) 및 tokio 비동기 런타임 채택으로 인해 원천적으로 발생하지 않는(equivalent) 상태**임이 추가로 확인되었습니다.
