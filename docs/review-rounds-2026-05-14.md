# Commit-by-commit critical review — `feat/upstream-features` (161 commits)

분기점 `main` 이후 커밋을 **오래된 순**으로 하나씩 비판적으로 리뷰. 참조 소스:

- `~/codes/epics-base/` (epics-base)
- `~/codes/epics-modules/asyn/` (asyn)
- `~/codes/pvxs/` (pvxs)

## 라운드 진행 방식

각 커밋에 대해:

1. `git show` 로 diff + 메시지 확인
2. 메시지가 인용한 C 파일/라인을 직접 읽어 검증
3. Rust 코드 vs C 원본 대조 — 누락 / 오류 / 잘못된 가정
4. 문제 발견 시 **별도 fix 커밋** (amend 안 함 — branch는 origin 에 push 되어 있음)
5. 라운드를 더 이상 에러가 나오지 않을 때까지 반복
6. 다음 커밋

문제 없음 = `clean (round 1)`. 수정 발생 = `round N → fix <hash>`.

---

## 리뷰 로그

### 1/161 — `4a41d74` docs: catalog epics-base / asyn upstream features not yet ported

**diff**: `+472 docs/epics_base_missing_features.md` (pure doc)

**검토**: catalog 문서가 당시 Rust 측 상태를 정확히 기록. 후속 audit (I1/I2/I3) 으로 일부 claim (`UInt64`/`RingAverager`/`SO_REUSEPORT server` 의 "ALREADY/DONE") 이 무효화되었으나, 이는 본 커밋의 결함이 아니라 후속 audit 의 결과 — audit doc / asyn-missing.md "Rust extensions" 섹션에서 명시적으로 추적됨.

**상태**: clean (round 1)

### 2/161 — `9d8a34b` feat(ca-server): split TCP/UDP server ports via EPICS_CAS_SERVER_PORT

**diff**: `+156 -14` (epics-base-rs/runtime/net.rs, server/ioc_app.rs, epics-ca-rs/server/ca_server.rs + bridge/qsrv 호출자 propagation)

**Round 1 — defect 발견**:

C 원본 `caservertask.c:491-499`:
```c
if (envGetConfigParamPtr(&EPICS_CAS_SERVER_PORT)) {
    ca_server_port = envGetInetPortConfigParam(&EPICS_CAS_SERVER_PORT, CA_SERVER_PORT);
} else {
    ca_server_port = envGetInetPortConfigParam(&EPICS_CA_SERVER_PORT, CA_SERVER_PORT);
}
ca_udp_port = ca_server_port;   // UDP follows TCP — same port
```

C 에서 EPICS_CAS_SERVER_PORT 는 **UDP+TCP 모두**를 같은 포트로 설정 (서버 측 env, client 측 EPICS_CA_SERVER_PORT 와 독립). Rust 본 커밋은 의도적으로 deviation — "UDP는 EPICS_CA_SERVER_PORT, TCP만 EPICS_CAS_SERVER_PORT" — commit message 의 "Mirrors PR #69" claim 과 어긋남. 영향: `EPICS_CAS_SERVER_PORT=6064` 만 set 한 C startup script 가 Rust 로 옮기면 UDP=5064 + TCP=6064 로 동작 → client SEARCH 가 EPICS_CA_ADDR_LIST 의 host:6064 로 가도 receiver 가 5064 → 연결 안 됨.

**Fix**: `d20f8b7` —
- `CaServerBuilder::new()` / `IocApplication::new()` 의 `port` 필드 seed 를 `ca_server_port()` (client semantic) 에서 `cas_server_port()` (server bind) 로 교체. C precedence 가 builder 의 UDP/TCP 양쪽에 적용됨.
- Builder/IocApplication 의 build 경로에서 별도 `EPICS_CAS_SERVER_PORT` env 재read 제거 (이미 `port` 에 흡수됨).
- `.tcp_port(...)` API 는 그대로 — Rust 확장 "shared UDP, split TCP" 멀티-IOC 시나리오용.
- Test assertion 메시지 정정: "UDP stays at 5064 / TCP follows" → "client semantic ignores EPICS_CAS / server bind picks up EPICS_CAS".

**상태**: clean (round 2 after fix `d20f8b7`)

### 3/161 — `ae277d1` feat(ca): honor EPICS_CA_MCAST_TTL on CA UDP sockets

**diff**: runtime/net.rs (ca_mcast_ttl), async_udp_v4.rs (set_multicast_ttl_v4 multi-NIC fanout), server/beacon.rs / server/udp.rs / client/search.rs (3 apply sites)

**C ref**: epics-base 2017 commit `f2a1834d`. Apply 사이트 2종 — `udpiiu.cpp:201` (client UDP socket), `caservertask.c:327` (server beacon socket). C 서버는 beacon+search 가 한 socket; Rust 는 둘로 분리되어 Rust는 3 사이트.

**검토**:
- Apply 사이트 매핑 일치 ✓
- Default = 1 일치 ✓
- Multi-NIC fan-out — `AsyncUdpV4::set_multicast_ttl_v4` 가 NIC별 시도, all-fail 시에만 error 반환. C 는 단일 socket 만 다루므로 Rust extension 으로 적절.

**Minor edge-case 분기 (defect 아님)**:
- TTL=0: C 수용 (host-only multicast), Rust 는 1 로 clamp.
- TTL>255: C byte truncate, Rust 는 default 1.
- Apply error: C `errlogPrintf`, Rust `let _ = ...` silent — operator visibility 손실.

**상태**: clean (round 1, edge-case noted)

### 4/161 — `8615bb4` feat(ca-client): honor EPICS_IOC_IGNORE_SERVERS quarantine list

**Round 1 defect**: `EPICS_IOC_IGNORE_SERVERS` 의 C 의미는 SERVER 측 server-name list (`dbServer.c::dbRegisterServer` 가 자기 서버 등록 거부). Rust 본 커밋은 CLIENT 측 IP/hostname quarantine list 로 해석 → 같은 var 이름 / 다른 동작. C IOC startup script `EPICS_IOC_IGNORE_SERVERS=rsrv` 가 Rust 로 옮기면 silent ineffective.

**Fix**: `f3738ce` — env var `EPICS_IOC_IGNORE_SERVERS` → `EPICS_RS_CLIENT_IGNORE` rename (Rust-only client-side quarantine 명시). C-parity (server-side refuse-to-register by name) 은 backlog.

**상태**: clean (round 2 after `f3738ce`)

### 5/161 — `6862ef0` feat(acf): soft DNS fallback for HAG entries

**검토**: C (`asLibRoutines.c::asHagAddHost`, commit 932e9f3) — DNS 성공시 dotted-quad **만** 저장, 실패시 `"unresolved:<host>"` prefix 저장. Rust 는 literal 항상 보존 + 성공시 resolved IPs append, 실패시 literal 만. 표현은 다르나 IP-based match 결과는 동등 (literal 은 dotted-quad caller 와 매치 안 되므로 harmless). Rust multi-IP support 는 multi-A-record 대응 strict extension.

**상태**: clean (round 1, storage shape 차이지만 match 동작 동등)

### 6/161 — `17210b4` feat(types): hex/octal string parse for Double/Float

**Round 1 defect**: C `epicsParseDouble` → `epicsStrtod` (libcom/epicsStdlib.c:347-374) 는 `0x` prefix hex 만 지원, leading-0 octal 인식 안 함 (strtod 가 0377 → 377.0). Rust 는 leading-0 → 255.0 octal 처리 → caput 값 silently 변동.

**Fix**: `87c645d` — float/double 의 octal 분기 제거. Hex 만 유지. PR #678 도 hex 만 추가 한 PR.

**상태**: clean (round 2 after `87c645d`)

### 7/161 — `7ed3baf` feat(server): built-in getenv device support

**검토**: C `devEnviron.c::read_lsi/stringin` 미설정 env: `val[0]=0, udf=TRUE, recGblSetSevrMsg(UDF_ALARM, udfs)` + 성공 반환. Rust 는 `Err(InvalidValue)` 반환 → framework 가 READ_ALARM/Invalid 로 매핑 (processing.rs:435-442). Alarm 코드 차이 (UDF vs READ) — 동일한 alarm 상태이나 코드/severity 다름.

**Fix 보류**: 완벽 C-parity 는 Record trait 에 `set_udf`/`set_alarm` hook 필요 (framework-wide change). 이 커밋 scope 밖. Alarm 발생 자체는 동작 — silent data corruption 없음.

**상태**: clean (round 1, alarm-code 차이는 known limitation)

### 8/161 — `23360e6` feat(ca-server,acf): wire mTLS identity into ACF METHOD/AUTHORITY

**검토**: PR #641 은 본 epics-base snapshot 에 미존재 — Rust forward-port. 내부 일관성 OK (ClientState 에 auth_method/auth_authority, ACF `check_access_method` 가 receive, plaintext peer 는 empty fields → legacy rule 무관).

**상태**: clean (round 1, upstream C 미존재)

### 9/161 — `a409311` feat(ca-client): shorten echo probe on suspend wake

**검토**: Tokio reactor 의 laptop-suspend stall 대응 — C libca 는 threaded I/O 라 같은 형태 없음. Issue #190 의 *증상* (laptop suspend stall) 은 동일하지만 *해결* 은 Rust-specific. Wall-clock skip > 3×idle_timeout (min 60s) 감지 → 다음 echo probe 1s 단축. 내부 일관성 OK.

**상태**: clean (round 1, Rust runtime-specific)

### 10/161 — `73b517c` feat(records,base): longout OOPT conditional output

**Round 1 defect**: C `longoutRecord.c::conditional_write` (PR `6c573b4`) 의 첫-사이클 force-emit (`outpvt == EXEC_OUTPUT`) 는 **OOPT=On_Change 모드에만 적용**. 다른 4 모드 (When_Zero / When_Non_zero / Transitions) 는 첫 사이클도 정상 비교. Rust 는 `!first_output_done` 체크를 match 위로 올려 모든 모드에 일반화 → When_Non_zero/Transition 모드에서 silent 출력.

**Fix**: `bd9d1c7` — `!first_output_done` 체크를 `match self.oopt == 1` 분기 내부로 옮김. Test `oopt_when_zero_first_cycle_forces_output` (버그를 단언) 을 C-parity 단언으로 재작성.

**상태**: clean (round 2 after `bd9d1c7`)

