# pvxs ↔ epics-pva-rs / epics-bridge-rs 기능 차이 분석

> 작성일: 2026-05-16 · **재검증: 2026-06-28 (§0.5 참조 — 현재 상태의 단일 출처)**
> 분석 대상:
> - **pvxs** (C++ 레퍼런스): `~/codes/epics-modules/pvxs` → 실제 위치 `~/codes/pvxs`,
>   `tls` 브랜치 tip `9beba6b` (2026-04-12). 최신 태그 `1.5.1` (2026-02), 릴리스노트는 `1.5.2 UNRELEASED`까지.
> - **epics-pva-rs** (Rust): workspace v0.17.2, `crates/epics-pva-rs`, ~22.5k LOC.
> - **epics-bridge-rs** (Rust): workspace v0.17.2, `crates/epics-bridge-rs`, ~11.3k LOC.
>
> 방법: pvxs **전체 커밋 1230개를 전수 분류**(5청크 멀티에이전트, FUNCTIONAL 483 / NON-FUNCTIONAL 747 — 부록 C),
> 머지된 PR·open/closed 이슈를 `gh`로 조사, pvxs C++ 소스와 두 Rust 크레이트의 소스를
> 멀티에이전트로 인벤토리하여 대조함. 전수 sweep은 본 문서의 격차 목록을 확정했으며
> §2~§5 외의 추가 기능 누락은 발견되지 않음(상세는 부록 C).

---

## 0.5 재검증 로그 (2026-06-28, workspace v0.20.3)

> 본 문서 본문(§0~§6, 부록)은 **2026-05-16 / v0.17.2 시점의 베이스라인**이다.
> 2026-06-28 caucus 재베이스라인 라운드에서 §2~§5를 현재 두 크레이트
> (v0.20.3) 소스와 pvxs `tls` 브랜치에 다시 대조했다. 결론: 본문 격차 표는
> **상당수 stale**(이후 구현됨)이며, 진짜 미해결분은 아래 잔존(residual)
> 목록으로 좁혀졌다. **현재 상태의 단일 출처는 이 로그**이며, 아래에 명시한
> 행은 본문 표보다 이 로그가 우선한다(베이스라인 표는 이력 보존용으로 둔다).

### 이번 라운드에서 닫은 격차 (per-feature 커밋, 모두 main, 미push)

| 커밋 | 닫은 본문 항목 | 근거 |
|---|---|---|
| `6eabcaae` | §2.6 `SSLKEYLOGFILE ❌` → ✅ | server+client rustls `config.key_log`를 `$SSLKEYLOGFILE` 설정 시 `KeyLogFile`로 배선(`auth/tls.rs`). pvxs `5db9222f` 대응 |
| `67b10ff1` | 부록 C "ID 충돌 탐지 `3b641bed` 채택 권장" → 채택됨 | SID/CID/IOID/searchID 카운터를 서로 다른 비-0 base에서 시작 |
| `29a0aa9e` | §2.8 info `-v` 자격증명 표시 없음 → ✅ | `pvinfo-rs -v`가 서버 자격증명 표시(pvxs `066ae597` 대응). `--show-credentials`와 독립 OR |
| `cfa75c58` | §4.1 `info(Q:form,...)` ⚠️확인필요 → ✅ | base `populate_display_info`가 Q:form info 태그를 7-name 메뉴로 `display.form` index에 매핑(pvxs `iocsource.cpp:42-62` 대응, VAL 한정) |
| `beffbb85` | §2.3 `serverInfo`/`server` PV 표기 정정 | Rust 서버가 `server_native::server_info::ServerInfoSource`로 `server` PV를 이미 호스팅함을 doc에 반영 |
| `3b1733b0` | §5-8 `ca_gateway` preload_path 주석 stale 정정 | lazy-resolution은 `install_search_resolver`로 동작 중; preload는 opt-in eager-prefetch |
| `cf52451c` | §4.2/§5-2 pvalink `MSS` 처분 정정 | `MSS→MS`는 **pvxs 자체 aliasing**(`pvalink.h:83-86` MSS 변형 없음, `pvalink_jlif.cpp:179-183` 명시 매핑). Rust 한계 아님 = 정확한 parity |

### 의도적 잔존 (residual) — 처분과 근거

| 항목 | 처분 | 근거 |
|---|---|---|
| `reExec`/`autoExec` Expert API (§2.1) | 잔존 (M, expert-only) | pvxs `reExecGet`/`reExecPut`는 전문가용 재실행 API. 영향 낮음, 일반 GET/PUT 경로는 동등 |
| `pvxsi` (QSRV iocsh target_information 덤프) | 잔존 (cosmetic) | 진단용 자격증명/타깃 정보 출력. 와이어/기능 영향 없음 |
| `pvxsr` (QSRV iocsh 서버 리포트, casr 대응) | **잔존 — 구조 변경 sign-off 대기** | 데이터(`PvaServer::report()→ServerReport`)는 존재하나, native `PvaServer`(peers/config/bound-port)가 `run_pva_server`(`runtime.rs:449`) **안에서 생성·`wait()`로 소비**되어 iocsh `register_fn` 경계(2계층 위)에서 핸들 접근 불가. 게다가 bound tcp/tls 포트는 bind **이후**에만 확정. casr는 `CaServer.stats:Arc<ServerStats>` 필드라 가능했지만, PVA는 진단 상태가 spawn된 태스크 내부에서 태어남. 채우려면 `ServerReportHandle`를 bind 직후 `run_pva_server`→`run_with_source`→등록 경계로 publish하는 **3개 함수 시그니처 변경 + 신규 cross-crate 공개 API** 필요 → "독립 변경 규모의 구조 수정"이라 사용자 승인 후 진행 |
| `EPICS_PVA_MAX_SEARCH_PERIOD` (§2.2, 이슈 #155) | 격차 아님 | pvxs도 제안 단계로 미구현 — 양쪽 모두 없음 |
| `SSLKEYLOGFILE` open-failure 진단 | 잔존 (minor) | `6eabcaae`는 enable 시 NOTICE 출력. pvxs는 파일 열기 실패 시 추가 Warning. rustls `KeyLogFile`이 열기/쓰기를 내부 처리하므로 실패 경고 경로만 미세 차이 |

> pvxsr를 제외한 나머지 잔존은 모두 cosmetic/expert-only/양쪽-공통 부재로,
> 와이어·기능 parity에 영향 없음. pvxsr만 실질 casr-격차이며 구조 변경
> 규모 때문에 승인 대기 상태로 둔다.

---

## 0. 요약 (TL;DR)

| 영역 | 상태 | 비고 |
|---|---|---|
| PVA 클라이언트 (GET/PUT/RPC/MONITOR/INFO) | ✅ 거의 동등 | `PUT_GET`/`PROCESS` 명령만 미구현 |
| PVA 서버 (native source / SharedPV) | ✅ 거의 동등 | `PUT_GET`/`PROCESS`는 "not supported" 응답 |
| pvData 타입 시스템 + NT 타입 | ✅ 동등 | NTNDArray/NTTable/NTURI/Union/Any 모두 있음 |
| 와이어 코덱 (segmentation/registry cache/bitset delta) | ✅ 동등 | 양방향 segmentation 재조립 구현됨 |
| TLS / 보안 | ⚠️ 부분 | pvxs는 PKCS#12 + X.509-name AuthZ, Rust는 PEM only·X.509 신원기반 인가 없음 |
| QSRV2 (IOC PVA 서버) | ⚠️ 부분 | group/single source 있음, **레코드 RPC·raw-frame 경로 누락** |
| pvAccess links | ⚠️ 부분 | pvxs 링크 옵션(`proc`/`sevr`/`defer`/`retry`/`atomic`/`monorder`...)의 일부만 동작 |
| CA gateway | ✅ pvxs 범위 밖 (별도 구현) | pvxs에 CA 게이트웨이 없음 — C++ `ca-gateway` 대비 |
| PVA gateway | ✅ pvxs 범위 밖 (별도 구현) | pvxs에 PVA 게이트웨이 없음 — C++ `pva2pva` 대비 |
| CLI 도구 | ⚠️ 부분 | 8개 존재하나 `pvput-rs -r` 무효·`pvlist-rs` PV 열거 미구현·`mshim-rs @iface` 무시 |

**가장 중요한 실질 격차 3가지**
1. **`PUT_GET`(cmd 12)·`PROCESS`(cmd 16) 명령 미구현** — pvxs도 `handle_PUT_GET`가 비어 있어 실제 격차는 작지만, 와이어 호환성상 명령 인지는 필요.
2. **QSRV2 어댑터(`QsrvPvStore`)가 RPC·`subscribe_raw`·`_checked` 변형 미지원** — pvxs QSRV2는 레코드/그룹 PV에 RPC를 지원함.
3. **TLS 신원 기반 인가 부재** — pvxs `tls` 브랜치는 X.509 CommonName을 ACF account로 매핑하지만 Rust는 PEM 로딩까지만.

---

## 1. pvxs 최근 진화 (커밋·PR·이슈 기반)

### 1.1 진행 중인 대형 작업 — TLS / "Secure PVAccess"

pvxs `tls` 브랜치 tip(`9beba6b`)은 **어떤 태그 릴리스에도 포함되지 않은** 약 2,500 LOC의 OpenSSL 기반 TLS 작업을 담고 있다.

- `98a7128` "Add TLS support w/ OpenSSL" — `src/ossl.cpp`/`ossl.h` 신규(~600 LOC), 클라이언트·서버 TLS, PKCS#12 자격증명 로딩, 신규 문서 `pkcs12.md`·`oscp.md`.
- `5db9222`/`fab1bfa` — `$SSLKEYLOGFILE` 동시 쓰기 처리 + Wireshark 복호화용 키 로깅.
- `066ae59` — `pvxinfo -v`가 서버 자격증명(X.509 peer 포함) 표시.
- `bd8986a`/`94546df`/`9beba6b` — 조건부 libssl 링크, python OpenSSL 빌드, `FILE*` 제거.

함의: 릴리스 1.5.x를 타깃하면 TLS는 필수가 아니지만, **다음 메이저에서 들어올 예정**. epics-pva-rs는 이미 `rustls` 기반 TLS를 선제 구현했으나 자격증명 모델이 다름(아래 §3.5).

### 1.2 테마별 머지 PR 요약

| 테마 | 대표 PR / 릴리스 |
|---|---|
| 프로토콜/와이어 | #27 `SetEndian` 처리, #62 "Deserialize Size", 1.3.0 Size↔Selector 모호성 해소, #32 TypeStore 유지 버그, 1.0.1 `CMD_MESSAGE`, #123 `ORIGIN_TAG 0.0.0.0` |
| 클라이언트 | #94 connect 실패 알림 지연, #85/#84 검색 재시도 step 리셋, #41 `poke()` 재정의, 1.4.1 disconnect 시 `Operation::name()` 크래시 회피 |
| 서버/QSRV | #37 "QSRV 2" 전면 재작성, #82/#81 포트 bind 충돌 조정, #98/#97 single source `DBE_ARCHIVE`, 1.5.0 채널 캐시 누수 수정, IOC C심볼 개명(`dbpvar→dbpvxr`) |
| pvAccess links | #57/#43 pva link 추가, 1.3.3 Union 필드 타깃 수정, 1.4.1 환경변수 존중 |
| 도구 | #22 멀티캐스트·IPv6, 1.5.0 `pvxvct` per-interface 멀티캐스트 |
| 빌드/플랫폼 | Python 3.11~3.13, #61 `TCP_NODELAY`, 1.5.1 macOS SIGPIPE(`SO_NOSIGPIPE`) |
| 회귀 수정 | 1.1.4 Compound delta sync, 1.4.1 주소목록 호스트명 조회 복원, #109 Union `pvaGetValue`, #108/#107 NTNDArray `Float32A` 오타 |

### 1.3 주목할 OPEN 이슈 (알려진 버그 / 미구현) — Rust 구현에도 시사점

| # | 제목 | Rust 관련성 |
|---|---|---|
| #161 | 느린 monitor 클라이언트(대용량 payload)에서 서버 evbuffer 무한 증가 | epics-pva-rs는 워터마크 기반 백프레셔가 있어 유리. 회귀 테스트 가치 |
| #176 | QSRV PUT마다 OS group 조회 → 성능 저하 | epics-bridge-rs도 PUT 경로의 ACF 조회 캐싱 점검 필요 |
| #156 | pvRequest 파서가 pvDataCPP와 비호환 (`field(v.a,v.b)` vs `field(v{a,b})`) | **Rust pvRequest 파서는 더 단순한 모델** — 중첩 brace 문법 비지원 (§3.4) |
| #177 | `time=true` pva link가 `userTag` 미복사 | epics-bridge-rs pvalink도 동일 점검 필요 |
| #173 | DNS/인터페이스 파생 상태 런타임 갱신 불가 | epics-pva-rs는 `reconfigure` 유무 점검 |
| #155 | `EPICS_PVA_MAX_SEARCH_PERIOD` 환경변수 제안 | Rust 검색 백오프 상한 env 미지원 |
| #87 | NTTable 필드가 삽입순 아닌 알파벳순 정렬 | Rust NTTable 컬럼 순서 점검 |
| #69 | scalar와 길이-1 배열 구분 불가 (프로토콜 한계) | 공통 한계 — 구현 차이 아님 |
| #44 | `pvxput`가 NTEnum 미이해 | Rust `pvput-rs`도 NTEnum 입력 점검 필요 |

### 1.4 프로토콜 코너 케이스 (호환성 주의)

- **엔디언**: 연결별 byte-order 플래그 존중 필수. pvxs는 big-endian 서버 비호환을 0.3.0에서 수정.
- **Size↔Selector 모호성**: 가변길이 Size 인코딩이 Union Selector와 혼동 가능 — TypeStore(타입 레지스트리) 유지가 정확해야 디코딩 성공.
- **null 문자열**: pvAccessJava의 "null" 문자열 인코딩 — interop 엣지케이스.
- **TCP inactivity timeout ≥ 40s**: pvAccessJava interop 요구.
- **Monitor create without initial ACK**: 1.4.0에서 명시적으로 처리한 코너 케이스.
- **`CMD_ORIGIN_TAG` 루프백 멀티캐스트 해킹**: unicast 검색을 `224.0.0.128`로 재멀티캐스트, loopback 경유분만 신뢰.

---

## 2. epics-pva-rs vs pvxs — PVA 프로토콜 코어

범례: ✅ 동등 · ⚠️ 부분 · ❌ 없음 · ➕ Rust 추가분

### 2.1 클라이언트 오퍼레이션

| 기능 | pvxs | epics-pva-rs | 비고 |
|---|---|---|---|
| GET | ✅ `clientget.cpp` GPROp | ✅ `op_get`/`op_get_raw` | |
| PUT | ✅ `fetchPresent` 포함 | ✅ `op_put`/`op_put_raw`/`op_put_value`/`op_put_field` | |
| RPC (NTURI) | ✅ `RPCBuilder` | ✅ `op_rpc`, `pvrpc` | |
| MONITOR | ✅ 큐·squash·pipeline | ✅ `op_monitor*` 패밀리, raw-frame 팬아웃 | |
| INFO / introspect | ✅ `InfoOp` CMD_GET_FIELD | ✅ `op_get_field`, `pvinfo` | |
| DISCOVER | ✅ `clientdiscover.cpp` | ✅ `discover`, `ping_all` | |
| `autoExec`/reExec Expert API | ✅ `reExecGet`/`reExecPut` | ⚠️ 명시적 reExec API 미확인 | 영향 적음 |
| 타입 변환/typed 헬퍼 | ✅ `as<T>`/`from<T>` | ✅ `pvget_typed`/`pvput_typed`/`pvmonitor_typed` (`TypedNT`) | |
| 배치 GET | ✅ | ✅ `pvget_many`/`pvget_many_full` | |
| connect/disconnect 콜백 | ✅ `connect()` Operation | ✅ `ConnectBuilder` `on_connect`/`on_disconnect` | |

### 2.2 클라이언트 연결 관리

| 기능 | pvxs | epics-pva-rs | 비고 |
|---|---|---|---|
| UDP 검색 + 재시도 백오프 | ✅ 30버킷 링 | ✅ `SearchEngine` | |
| Beacon 리스닝 + GUID-flap 억제 | ✅ 5분 규칙 | ✅ `beacon_throttle.rs` `BeaconTracker` | |
| TCP name servers | ✅ `EPICS_PVA_NAME_SERVERS` | ✅ 동일 env | |
| 채널 상태머신 | ✅ Searching/Connecting/Creating/Active | ✅ Searching/Connecting/Active/Reconnecting | |
| Echo keep-alive | ✅ `[1,15]s` | ✅ heartbeat | |
| 재연결 시 monitor 재INIT/START | ✅ | ✅ | |
| IPv4/IPv6 듀얼 + 멀티캐스트 | ✅ | ✅ | |
| `hurry_up`/`cache_clear`/`ignore_server_guids` | ✅ `hurryUp`/`cacheClear`/`ignoreServerGUIDs` | ✅ | |
| `EPICS_PVA_MAX_SEARCH_PERIOD` 상한 | ❌ (이슈 #155 제안 단계) | ❌ | 둘 다 없음 |

### 2.3 서버

| 기능 | pvxs | epics-pva-rs | 비고 |
|---|---|---|---|
| GET/PUT/MONITOR/RPC/GET_FIELD 서빙 | ✅ | ✅ `server_native/tcp.rs` | |
| Monitor pipeline / flow-control | ✅ window/limit/watermark | ✅ `PipelineOptions`, nack window | |
| Source 인터페이스 | ✅ `Source` 가상 | ✅ `ChannelSource` trait + 타입스테이트 ACF 게이트 | |
| SharedPV (mailbox/readonly) | ✅ `buildMailbox`/`buildReadonly` | ✅ `SharedPV`/`SharedSource` | |
| 다중 source 우선순위 | ✅ `(order,name)` | ✅ `CompositeSource` | |
| Beacon 송출 (short/long) | ✅ 15s×10 → 180s | ✅ short/long, IPv6 `ff0e::400` | |
| 연결 한도 | ✅ — | ✅ `MAX_CONNECTIONS`/`_CHANNELS_PER_CONN`/`_OPS_PER_CHANNEL` | ➕ Rust가 더 명시적 |
| TX 백프레셔 | ✅ `tcp_tx_limit` backlog deque | ✅ 워터마크 기반 | 이슈 #161 대비 Rust 유리 |
| `serverInfo`/채널목록 RPC | ✅ `__server` source ( `server` PV) | ❌ 미구현 | **격차** — pvlist 응답용 |
| RPC 서비스 프레임워크 | (사용자 코드) | ➕ `#[pva_service]` 매크로, `PvaService` trait | Rust 편의 추가 |

### 2.4 데이터 모델 / NT 타입

| 항목 | pvxs | epics-pva-rs |
|---|---|---|
| 스칼라 11종 + 배열 | ✅ | ✅ `ScalarType` |
| Struct/Union/Any + 배열변형 | ✅ `StructA`/`UnionA`/`AnyA` | ✅ `FieldDesc` 전체 코드공간 |
| zero-copy 배열 | ✅ `shared_array` | ✅ `ScalarArrayTyped` (`Arc<[T]>`) |
| BitMask 변경추적 | ✅ `BitMask` | ✅ `BitSet` |
| NTScalar/NTScalarArray | ✅ | ✅ |
| NTEnum | ✅ | ✅ `enum_t` |
| NTNDArray 1.0 | ✅ (codec/compressedSize) | ✅ `NtNdArray` (`NdCodec`/compressedSize) |
| NTTable | ✅ `add_column` | ✅ `table.rs` |
| NTURI | ✅ `call()` 헬퍼 | ✅ `uri.rs` |
| NTAttribute | ✅ | ✅ 1.1 |
| TimeStamp/Alarm | ✅ | ✅ `meta.rs` |
| `#[derive(NTScalar/NTTable)]` | — | ➕ Rust 매크로 |
| NTMatrix | ❌ | ❌ (stale README가 언급하나 모듈 없음) |

### 2.5 프로토콜 / 코덱

| 기능 | pvxs | epics-pva-rs | 비고 |
|---|---|---|---|
| 8-byte 헤더, magic `0xCA`, ver 2 | ✅ | ✅ `proto/header.rs` | |
| 제어 메시지 SetMarker/AckMarker/SetEndian | ✅ | ✅ `ControlCommand` | |
| 앱 명령 0~22 | ✅ 전체 정의 | ✅ `Command` 0~22 (`OriginTag`=22 포함) | |
| Size/Selector 가변길이 인코딩 | ✅ | ✅ `size.rs`/`selector.rs` | 이슈 #62/#156 모호성 |
| 타입-introspection 레지스트리 캐시 (`0xfd`/`0xfe`) | ✅ `TypeStore` | ✅ `EncodeTypeCache`/`TypeCache` | 와이어 호환 필수 — 구현됨 |
| BitSet delta 인코딩 | ✅ `to_wire_valid` | ✅ `encode_pv_field_with_bitset` | |
| 메시지 segmentation | ⚠️ **디코드 전용** (송신은 비분할) | ✅ **양방향 재조립** (P-G20/P-G21) | ➕ Rust가 더 완전 |
| Echo heartbeat | ✅ | ✅ | |
| OriginTag prefix (멀티캐스트 포워딩) | ✅ | ✅ `build_origin_tag_prefix`/`try_peel_origin_tag` | |
| `PUT_GET`(12) | ❌ `handle_PUT_GET` 비어있음 | ❌ "not supported" 응답 | 양쪽 모두 사실상 미구현 |
| `PROCESS`(16) | ❌ | ❌ | process는 `record[process=true]` 옵션으로 |
| `ARRAY`(14)/`MULTIPLE_DATA`(19)/`ACL_CHANGE`(6) | ❌ 정의만 | ⚠️ 인지하나 무동작 | |

### 2.6 인증 / TLS

| 기능 | pvxs (`tls` 브랜치) | epics-pva-rs | 비고 |
|---|---|---|---|
| "ca" 익명/유저 인증 | ✅ `{user,host}` 구조체 | ✅ `auth/plain.rs` (`authnz_default_user/host`) | |
| OS 그룹 → role | ✅ `osdGetRoles` | ✅ `posix_groups` (Unix only) | |
| TLS 전송 | ✅ OpenSSL `bufferevent_openssl_*` | ✅ `rustls` 0.23 (`auth/tls.rs`) | 라이브러리 다름 |
| 키체인 포맷 | ✅ **PKCS#12** (`PKCS12_parse`) | ❌ **PEM 번들 only** | **격차** |
| 키체인 비밀번호 | ✅ `;password` 접미사 | ⚠️ env 인식하나 미사용 | **격차** |
| X.509 신원 → 인가 account | ✅ `fill_credentials` (CN→account, root CA CN→authority) | ❌ 없음 | **격차** — SPVA 핵심 |
| client_cert require/optional | ✅ TLS options 문자열 | ⚠️ 옵션 일부 | |
| `SSLKEYLOGFILE` | ✅ | ❌ | 디버깅 편의 |
| 인증서 관리 데몬 (`pvacms` 류) | ❌ (이 체크아웃엔 없음, 후속 작업) | ❌ | 둘 다 없음 |

### 2.7 설정 (환경변수)

epics-pva-rs는 클라이언트/서버 핵심 env를 모두 처리: `EPICS_PVA_ADDR_LIST`, `_AUTO_ADDR_LIST`, `_INTF_ADDR_LIST`, `_BROADCAST_PORT`, `_SERVER_PORT`, `_NAME_SERVERS`, `_CONN_TMO`, 서버측 `EPICS_PVAS_*` 전체 + `EPICS_PVAS_BEACON_PERIOD`/`_LONG`, `MAX_CONNECTIONS` 등. `EPICS_PVAS_*`가 미설정 시 `EPICS_PVA_*`로 폴백하는 pvxs 규칙도 동일.

- pvxs 고유: `EPICS_PVA_CONN_TMO`에 `tmoScale=4/3` 적용(30s→40s 유효). Rust 기본값 40s — **결과는 동일하나 스케일 로직 확인 필요**.
- pvxs 고유: `EPICS_PVAS_AUTO_BEACON_ADDR_LIST` 등 일부 변형. Rust도 `_AUTO_BEACON_ADDR_LIST` 처리함.

### 2.8 CLI 도구

| 도구 | pvxs | epics-pva-rs | 격차 |
|---|---|---|---|
| get | `pvxget` (`-r`/`-#`/`-F`) | `pvget-rs` (`-r`/`-M`/`-v`/`-w`) | 동등 |
| put | `pvxput` | `pvput-rs` | ⚠️ **`-r` 플래그가 무효** (파싱만, 미적용); pvxs #44처럼 NTEnum 입력 점검 필요 |
| monitor | `pvxmonitor` | `pvmonitor-rs` | 동등 |
| info | `pvxinfo` (`-D`/`-v`) | `pvinfo-rs` (`-w`) | ⚠️ `-v` 서버 자격증명 표시 없음 |
| list | `pvxlist` (`-A`/`-p`/`-i`) | `pvlist-rs` (`--ping`) | ⚠️ **`pvxlist <ip>` PV 열거 미구현** (discovery 스트림만) |
| call (RPC) | `pvxcall` | `pvcall-rs` | 동등 |
| pvxvct | `pvxvct` (`-C`/`-S`/`-B`/`-H`/`-P`) | `pvxvct-rs` | ⚠️ 필터 플래그 범위 확인 필요 |
| mshim | `pvxmshim` (`-L`/`-F`/`-p`) | `mshim-rs` | ⚠️ **`@iface`·`,ttl#` 문법 파싱만 하고 무시** |

---

## 3. epics-pva-rs 측 미해결 격차 (UNFIXED 목록)

> 글로벌 룰에 따라 워크어라운드/미구현을 명시적으로 나열함.

1. **`PUT_GET`(cmd 12)·`PROCESS`(cmd 16) 미구현** — 서버는 `handle_unsupported_op`로 "not supported" 응답, 클라이언트에 `op_put_get`/`op_process` 없음. pvxs도 비어 있어 실질 격차는 작으나 와이어 명세상 명령 인지는 유지됨.
2. **`build_put_value`가 비-Scalar/비-`value`-Structure descriptor에 대해 실패** — union/variant/structure-array value 필드 PUT 시 `InvalidValue("PUT not supported for descriptor ...")` (`ops_v2.rs:1899`).
3. **pvRequest 파서가 단순 모델** — `field(...)` selector + `record[k=v]` 옵션만. pvDataCPP의 중첩 brace 문법(`field(v{a,b})`)·중첩 옵션 그룹 비지원. pvxs 자체도 비표준(이슈 #156)이라 "완전 호환" 대상이 불명확.
4. **`pvput-rs -r` 무효** — CLI parity용으로 받기만 하고 오퍼레이션에 미적용.
5. **`pvlist-rs` PV-이름 열거 미구현** — 서버 discovery 스트림만 노출.
6. **`mshim-rs` `@iface`/`,ttl#` 미적용** — 파싱·로깅만, 기본 경로만 사용.
7. **TLS: PKCS#12 미지원·키체인 비밀번호 미사용·X.509 신원기반 인가 없음** (§2.6).
8. **서버 `serverInfo`/채널목록 RPC 없음**.
9. **README.md stale** — `protocol.rs`/`pvdata.rs`/`serialize.rs` 평면 레이아웃과 4-도구 CLI를 기술하나 실제는 `proto/`·`pvdata/`·`client_native/` 트리 + 8개 바이너리.
10. `pvdata/encode.rs:138` — "obscure encodings"용 제네릭 폴백 경로 미완.

런타임 `unimplemented!()`/`todo!()`/패닉 스텁은 없음 — 위 격차는 모두 명시적 타입 에러 반환 또는 문서화된 후속작업.

---

## 4. epics-bridge-rs vs pvxs — IOC 통합 & 게이트웨이

epics-bridge-rs는 pvxs의 범위(`ioc/` QSRV2 + pvalink)를 넘어 CA/PVA **게이트웨이**까지 포함한다. pvxs 자체에는 게이트웨이가 없으므로, 게이트웨이는 C++ `ca-gateway`·`pva2pva`가 레퍼런스다.

### 4.1 QSRV2 (IOC PVA 서버) — pvxs `ioc/` 대비

| 기능 | pvxs QSRV2 | epics-bridge-rs `qsrv/` | 격차 |
|---|---|---|---|
| Single source (레코드→PV) | ✅ `singlesource.cpp` | ✅ `BridgeChannel` (NTScalar/NTEnum/NTScalarArray) | |
| Group source (구조화 PV) | ✅ `groupsource.cpp` | ✅ `GroupChannel`/`GroupMonitor` | |
| Group JSON: `+id`/`+atomic`/`+type`/`+channel`/`+trigger`/`+putorder` | ✅ | ✅ `group_config.rs` (+ `+nsecmask`/`+value`) | |
| `+type` 종류 (scalar/plain/meta/any/proc/structure/const) | ✅ 6종 | ✅ `FieldMapping` 6종 | |
| `info(Q:group,...)` 인라인 + `dbLoadGroup` 파일 | ✅ | ✅ `parse_info_group`/`parse_group_config` | |
| `info(Q:form,...)` display.form 힌트 | ✅ Default/String/Binary/Hex/... | ⚠️ 확인 필요 (`pvif.rs`) | 점검 필요 |
| DBE 이벤트 (VALUE/ALARM/PROPERTY, `record._options.DBE`) | ✅ value+property 이중 구독 | ⚠️ `BridgeMonitor` overflow 카운트 — DBE 마스크 오버라이드 확인 필요 | 점검 필요 |
| atomic group put (다중 lock) | ✅ `DBManyLocker` | ✅ 모든 레코드 lock 동시 보유 | |
| `record._options.block`/`process` (dbProcessNotify) | ✅ | ⚠️ `ProcessMode`/`PutOptions` — `block` 경로 확인 필요 | 점검 필요 |
| 알람 매핑 / `DESC`→display.description | ✅ `getTimeAlarm`/`getProperties` | ✅ `convert.rs` alarm/timestamp/display/control | |
| Access Security (ACF) 통합 | ✅ `SecurityClient` + `asTrapWrite` 감사 | ✅ `AcfAccessControl` | asTrapWrite 감사 동등성 확인 필요 |
| **레코드/그룹 PV에 RPC** | ✅ | ❌ `QsrvPvStore`가 `rpc` 미구현 | **격차** |
| **raw-frame 모니터 fast-path** | (서버 일반) | ❌ `QsrvPvStore`에 `subscribe_raw` 없음 | **격차** |
| **ctx-aware `_checked` 접근검사 변형** | ✅ | ❌ `QsrvPvStore`에 없음 | **격차** |
| const 배열에 중첩 배열/구조 | ✅ | ❌ 거부 (`group_config.rs` 한계) | 경미 |
| IOC shell: `dbLoadGroup`/`processGroups`/그룹 리스트 | ✅ `pvxgl`/`pvxsl`/`pvxsr` 등 | ⚠️ `dbLoadGroup`/`processGroups`/`qsrvStats`/`resetGroups` — pvxs 명령군과 1:1 아님 | 명령 이름 불일치 |

> **핵심 격차**: `QsrvPvStore`가 구형 `ChannelSource` 모양(`list_pvs`/`has_pv`/`get_introspection`/`get_value`/`put_value`/`is_writable`/`subscribe`)만 구현하고 `rpc`·`subscribe_raw`·`_checked` 변형이 없음. PVA *게이트웨이* source는 이를 모두 갖췄으므로, QSRV 어댑터를 동일 trait 형태로 끌어올리면 해소됨.

### 4.2 pvAccess links — pvxs `ioc/pvalink*` 대비

pvxs pvalink는 풍부한 JSON 링크 옵션을 가진다. epics-bridge-rs `pvalink/`는 일부만 동작한다.

| 링크 옵션 | pvxs | epics-bridge-rs | 격차 |
|---|---|---|---|
| `pv` (PV명) / 바레 문자열 | ✅ | ✅ `@pva://PV?...` 문법 | 문법 표기 다름 |
| `field` (하위 필드) | ✅ | ✅ | |
| `Q` (monitor 큐 크기) | ✅ | ⚠️ `monitor` 옵션 — `Q` 정수 매핑 확인 필요 | |
| `pipeline` | ✅ | ⚠️ 확인 필요 | |
| `proc` (NPP/PP/CP/CPP) | ✅ | ✅ `proc` 쿼리 옵션 | |
| `sevr` (NMS/MS/MSI/MSS) | ✅ | ❌ `MS`/`NMS` 파싱 후 **무시** (PVA 경로 무효) | **격차** |
| `time` (원격 timeStamp 복사) | ✅ (단 `userTag`는 이슈 #177) | ⚠️ 확인 필요 | |
| `monorder` | ✅ -1024..1024 | ❌ 미확인 | **격차 가능** |
| `defer` (값 큐잉 후 지연 Put) | ✅ | ❌ 미확인 | **격차 가능** |
| `retry` (연결 끊김 중 Put 큐잉) | ✅ | ❌ 미확인 | **격차 가능** |
| `always` (변화 없어도 CP 처리) | ✅ | ❌ 미확인 | **격차 가능** |
| `local` (로컬 채널 강제) | ✅ | ❌ 미확인 | **격차 가능** |
| `atomic` (원자 다중링크) | ✅ | ❌ 미확인 | **격차 가능** |
| disconnect → `LINK_ALARM/INVALID` | ✅ | ✅ `alarm_message` | |
| IOC shell (`dbpvxr`/링크 덤프) | ✅ `dbpvxr` | ✅ `dbpvxr`/`pvxr`/`pvxrefdiff`/`pvalink_enable` | |
| INP-monitor 레코드 재처리 (`scan_on_update`) | ✅ CP/CPP scan | ⚠️ `notify_tx`가 `#[allow(dead_code)]` — 수신측 미연결 | **격차** |

### 4.3 CA Gateway — C++ `ca-gateway` 대비 (pvxs 범위 밖)

epics-bridge-rs `ca_gateway/`는 C++ `ca-gateway`를 정조준한 별도 구현. pvxs에는 해당 기능이 없다.

- ✅ 채널 캐시 5-state FSM(`Dead/Connecting/Inactive/Active/Disconnect`), `.pvlist` 전체 파서(EVALUATION ORDER/ALLOW/DENY/ALIAS/정규식/`\0`-`\9` backreference/`DENY FROM host`), ACF 접근제어, upstream 재구독·백오프, putlog(OK/DENIED/FAILED + 100MiB 회전), 12 native + 10 C-호환 stat PV, SIGUSR1 명령파일(`R1/R2/R3/AS/PVL/VERSION`), 슈퍼바이저.
- ⚠️ **미구현(의도적)**: `fd`(open-fd 카운트), RATE_STATS 내부치(`clientEventCount`/`postEventCount`/`loopCount` — C++ 이벤트루프 종속).
- ⚠️ connection-event broadcast `Lagged` 시 재생(replay) 없음 — subscriber refcount 일시 오차 가능 (후속작업으로 문서화됨).
- ⚠️ upstream측 TLS 미배선 (downstream TLS 종단만 `ca-gateway-tls` 피처로 존재).

### 4.4 PVA Gateway — C++ `pva2pva` 대비 (pvxs 범위 밖)

epics-bridge-rs `pva_gateway/`는 C++ `pva2pva/p2pApp` 대비 별도 구현.

- ✅ 채널 캐시(upstream monitor 1개 → `tokio::broadcast` 팬아웃), cleanup tick, `MAX_ENTRIES` 5만 상한(DoS 방어), 음성결과 LRU 캐시, **F-G12 raw-frame 포워딩**(MONITOR DATA 바디 바이트 verbatim, 재인코딩 0회), RPC 패스스루(p2pApp 주요 공백 해소), 게이트웨이측 ACF + per-PV `AsgResolver`, downstream 자격증명을 upstream으로 전달하는 per-(account,method) 클라이언트 풀(PG-G10), 워터마크→upstream-pause 전파(PG-G9).
- ✅ tower식 미들웨어(`ReadOnlyLayer`/`AclLayer`/`AuditLayer`), 멀티테넌트(N upstream × M downstream), 진단 control PV.
- ⚠️ control PV가 **읽기 전용** — 쓰기형 제어(엔트리 드롭/캐시 flush/설정 reload)는 범위 밖 ("credentialed RPC surface 필요").
- ⚠️ 미들웨어 `AclLayer`가 **glob-only** (정규식 아님; 정규식은 CA 게이트웨이 `.pvlist` 전용).

---

## 5. epics-bridge-rs 측 미해결 격차 (UNFIXED 목록)

1. **`QsrvPvStore`에 RPC·`subscribe_raw`·`_checked` 변형 없음** — QSRV2-서빙 레코드/그룹 PV가 PVA RPC·raw-frame fast-path·컨텍스트 접근검사 미지원. (가장 실질적인 pvxs QSRV2 대비 격차)
2. **pvalink `MS`/`NMS` 최대-심각도 플래그 파싱 후 폐기** — PVA 경로에 효과 없음.
3. **pvalink `notify_tx`가 dead code** — INP-monitor 레코드 알림 채널의 수신측 미연결 → monitor 구동 `scan_on_update`/`notify`가 소유 레코드로 푸시백 안 됨.
4. **pvalink `monorder`/`defer`/`retry`/`always`/`local`/`atomic`/`Q`/`pipeline` 옵션** — 구현 여부 코드 확인 필요(현 인벤토리에서 미확인 = 미구현 가능성). pvxs는 전부 지원.
5. **CA gateway**: `fd`·RATE_STATS 일부 stat 미구현(의도적), connection-event `Lagged` 재생 없음, upstream TLS 미배선.
6. **PVA gateway**: control PV 읽기전용, 미들웨어 ACL glob-only.
7. **QSRV const 배열에 중첩 배열/구조 거부**, JSON `null` const 거부.
8. **`lib.rs` doc stale** — `ca_gateway`/`pvalink`을 "planned"로 표기하나 실제 구현됨. `downstream.rs`의 lazy-resolution 불가 주석과 `preload_path` 설명도 stale(실제로 `install_search_resolver`가 동작함).

런타임 스텁/`unimplemented!()` 없음.

---

## 6. 우선순위 권고

### epics-pva-rs

| 우선 | 항목 | 근거 |
|---|---|---|
| 高 | pvRequest 파서 — pvDataCPP 중첩 brace 문법 호환 (이슈 #156) | interop 정확성 |
| 高 | 서버 `serverInfo`/채널목록 RPC | `pvlist <ip>` 동작·운영 가시성 |
| 中 | `pvput-rs -r` 적용, `pvlist-rs` PV 열거, `mshim-rs @iface` | CLI parity |
| 中 | TLS: PKCS#12 로딩 + 키체인 비밀번호 + X.509-name AuthZ | pvxs `tls` 브랜치가 메인라인 진입 예정 |
| 低 | `PUT_GET`/`PROCESS` 명령 — pvxs도 미구현, 우선순위 낮음 | 와이어 명령 인지만 유지 |
| 低 | `EPICS_PVA_MAX_SEARCH_PERIOD` (이슈 #155) | pvxs도 제안 단계 |

### epics-bridge-rs

| 우선 | 항목 | 근거 |
|---|---|---|
| 高 | `QsrvPvStore`를 native `ChannelSource` trait로 승격 — RPC·`subscribe_raw`·`_checked` 추가 | QSRV2 대비 핵심 격차 |
| 高 | pvalink 옵션 완성 — `monorder`/`defer`/`retry`/`always`/`local`/`atomic`/`Q`/`pipeline` 검증·구현 | pvxs pvalink parity |
| 中 | pvalink `notify_tx` 수신측 배선 → `scan_on_update`/CP 처리 완성 | 링크 의미론 정확성 |
| 中 | pvalink `time=true` 시 `userTag` 복사 (pvxs 이슈 #177 동일 점검) | |
| 中 | QSRV PUT 경로 OS-group 조회 캐싱 (pvxs 이슈 #176 동일) | 성능 |
| 低 | CA gateway upstream TLS 배선, PVA gateway 쓰기형 control PV | 운영 편의 |

---

## 부록 A. 분석 대상 버전·경로

- pvxs: `~/codes/pvxs`, `tls` 브랜치, tip `9beba6b1722f9f28b2e4cb93a994482af3c23b31` (2026-04-12), 커밋 1230개, 태그 ~`1.5.1`.
- epics-pva-rs: `crates/epics-pva-rs`, workspace v0.17.2, src ~22.5k LOC / 70 파일.
- epics-bridge-rs: `crates/epics-bridge-rs`, workspace v0.17.2, src ~11.3k LOC / 43 파일.

## 부록 B. "동등"의 한계

본 문서의 ✅는 *기능 존재*를 의미하며 *바이트 단위 와이어 호환성·코너케이스 동등성*을 보증하지 않는다. 특히 §1.4 프로토콜 코너 케이스(엔디언, Size/Selector 모호성, null 문자열, monitor-without-ACK, ORIGIN_TAG)는 별도의 interop 테스트로 검증해야 한다. ⚠️/❌ 중 "확인 필요"로 표기된 항목은 본 인벤토리 수준에서 단정하지 못한 부분으로, 해당 모듈 코드 정독이 필요하다.

## 부록 C. 전수 커밋 분류 결과 (1230개)

pvxs 커밋 1230개(`9beba6b` ~ `466044d6 initial`)를 250개 단위 5청크로 나눠 멀티에이전트가 한 줄도 빠짐없이 분류함. FUNCTIONAL = PVA 프로토콜/와이어·클라이언트·서버·QSRV·pvalink·데이터모델·코덱·TLS·도구·설정·네트워킹 동작 변경. NON-FUNCTIONAL = 빌드/CI/문서/테스트/버전범프/포맷팅.

| 청크 | 범위 | 커밋 | FUNCTIONAL | NON-FUNCTIONAL |
|---|---|---|---|---|
| 00 | 최신 (2026~) | 250 | 86 | 164 |
| 01 | | 250 | 79 | 171 |
| 02 | | 250 | 89 | 161 |
| 03 | | 250 | 95 | 155 |
| 04 | 최초 (2019, `initial`) | 230 | 134 | 96 |
| **합계** | | **1230** | **483** | **747** |

### 전수 sweep으로 확정/추가된 사항

- **subsystem 최초 등장** (청크 04): codec/UDP `466044d6 initial`(2019-10), shared_array `4c60d72f`, 데이터모델 `801d295c "start PVD"`, BitMask `8fca41b6`, NT 타입 `a207a54e "start NT"`, 서버 `cf64dad1 "start on server"`, 서버 모니터 `c2a4224a`, 클라이언트 `1edeab8a "start client"`, SharedPV `cd2d9265`. QSRV2/pvalink/TLS는 이 범위(초기)에 없음.
- **QSRV2 도입**: `93e4d3ee "QSRV 2 prototype"` → `afafa095 "revise QSRV 2 prototype"`, soft IOC `006a3202 softIocPVX`. 이후 group put/process 정합성 수정 다수(`c06d4bb6` `+putorder` 강제, `0b0dfde5` 무효 group put 에러, `59c7fde9` over-process 수정, `46ee1a69` ACF any_of).
- **pvalink 도입**: `1dcdd8e6` import → `c0093860`/`5f483258`/`6d1216da` porting part 1~3, 심볼 개명 `9a13662e dbpvar→dbpvxr`·`f764e00e pvaLinkNWorkers→pvxLinkNWorkers`·`cb627971 lsetPVA→lsetPVX`.
- **TLS 도입**: `98a71280 "Add TLS support w/ OpenSSL"` 외 `bd8986ae`/`066ae597`/`5db9222f`/`9beba6b1` — 전부 `tls` 브랜치, 태그 릴리스 미포함(§1.1 확정).
- **epics-pva-rs가 이미 따라잡은 항목** (전수 sweep로 교차확인): `058b3c91 $PVXS_ENABLE_IPV6` → Rust `enable_ipv6_udp`(`server_native/udp.rs`), `a064677e SO_RXQ_OVFL` UDP 드롭 카운터 → Rust `enable_so_rxq_ovfl_for_socket`(소스 주석에 커밋 `a064677e` 명시), `cce797263d`/`ec8d0df1b3` SetEndian/byte-order → Rust `proto/header.rs`. **즉 §2 비교표의 ✅ 판정이 커밋 이력으로도 뒷받침됨.**
- **연결 공정성**: `e741e411` — 한 피어에서 연속 4개 메시지만 처리 후 이벤트루프 양보. Rust는 연결별 tokio task 모델이라 런타임이 공정성 담당 — 구조가 달라 직접 격차 아님.
- **ID 충돌 탐지**: `3b641bed` — SID/CID/IOID 카운터를 서로 다른 비-0 base에서 시작해 ID 타입 혼동 탐지. Rust 구현 시 채택 권장(경미).
- 전수 검토 결과 §2~§5의 격차 목록 외 **추가 기능 누락은 발견되지 않음**. 이전 샘플링(`head -80`) 대비 새 격차 0건 — 두 Rust 크레이트가 pvxs 기능 표면을 폭넓게 따라왔음을 확인.

### 분류 경계 판정 (재현성)

청크 에이전트가 NON-FUNCTIONAL로 분류한 경계 사례: `ff3c0e4d`(pvRequest 파서의 `std::regex` 제거 — 동작 불변 리팩터), `e9d27604`(config 헤더 통합 — 동작 불변), `c197ad6a`(QSRV 문서). 이들을 FUNCTIONAL로 옮기면 합계가 ±2~3 변동하나 §2~§5 결론에는 영향 없음.
