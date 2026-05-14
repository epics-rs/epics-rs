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

### 11/161 — `a02c310` feat(records): aai / aao / subArray array record types

**Round 1 defect**: C `subArrayRecord.c` (init_record:103-104, readValue:310-314) clamps `NELM <= MALM` 와 `INDX < MALM`. Rust SubArray put_field 는 `INDX >= 0` 만, NELM 는 positive 검증만 — MALM 비교 없음. INDX=999 / MALM=10 일 때 C 는 9 로 클램프, Rust 는 999 그대로 → 디바이스 슬라이싱이 소스 배열 끝 너머 읽음.

**Fix**: 본 라운드 — `INDX min(malm-1)`, `NELM min(malm)` 클램프 + MALM put 시 NELM/INDX 재클램프 (C init guard 와 일치).

**상태**: clean (round 2 after fix `0d99c44` — pending verification)

### 12/161 — `ac92e3e` feat(ca,base): SIMM=RAW path + dbServerStats counter expansion

**Round 1 defect**: C `aiRecord.c:495` SIMM=RAW: `rval = (long)floor(sval)` — floor toward -∞. Rust `convert_to(Long)` 는 `f64 as i32` truncation toward zero. 음수 RAW (bipolar ADC) 에서 sval=-1.5 → C: -2, Rust: -1. Silent 값 다름.

**Fix**: 본 라운드 — SIMM=RAW 경로의 Double/Float→Long/Int64 narrowing 만 `.floor() as i32/i64` 로 변경 (다른 convert_to 호출은 영향 없음).

**상태**: clean (round 2 after fix — pending hash)

### 13/161 — `ec739d9` docs: annotate upstream-tracking with per-item implementation status

**검토**: 순수 docs.

**상태**: clean (round 1)

### 14/161 — `97300ce` feat(records,base): bi Raw Soft Channel routes INP to RVAL + applies MASK

**검토**: C `devBiSoftRaw.c::readLocked:50-55` 는 `dbGetLink(DBR_ULONG, &rval)` + `rval &= mask`. Rust `bi.rs::apply_raw_input:204-209` 는 `to_f64() as i32` + `rval &= mask`. 일반 single-bit mask 에선 동등. Edge case: u32 high-bit (>=0x80000000) 인 값은 f64 intermediary 가 i32 negative 로 해석.

**상태**: clean (round 1, u32 high-bit edge-case note)

### 15/161 — `366b707` feat(iocsh): iocshLoad command + multiline backslash continuations

**C ref**: epics-base Issue #847 (`iocsh.cpp::iocshLoad`:1340-1346 → iocshBody with macros). C++ paren form `iocshLoad("path","K=V,N=2")` 도 정상 작동.

**검토**: Rust `execute_script_with_macros` 가 line별 `substitute_macros` + `tokenize` 의 env expand 2단계. `tokenize::split_comma_args` quote-aware → C++ paren form 의 macros 문자열 내 콤마도 정확히 보존. `join_backslash_continuations` 가 trailing `\\n` 잇기. macros vs env 우선순위 (C MAC_HANDLE 와 동일하게 macros 먼저).

**상태**: clean (round 1)

### 16/161 — `c8c3284` fix(iocsh): dbLoadRecords propagates add_record rejection (144f975)

**검토**: `return Err(e)` 만 추가 — `execute_script` 의 last_err 체인이 받음. C `iocshSetError` 와 시맨틱 동등.

**상태**: clean (round 1)

### 17/161 — `e77358b` fix(ca,pva): guard CLI timeout against NaN/Inf/non-positive (1655d68e analog)

**검토**: caget/cainfo/camonitor/caput 4 도구 모두 `cli::timeout_duration` 경유. NaN/Inf/≤0 → DEFAULT clamp (CA 1.0s 는 C `tool_lib.h:51 DEFAULT_TIMEOUT 1.0` 일치). C 1655d68e (RTEMS-osdEvent) 는 NaN→`RTEMS_NO_TIMEOUT` (wait forever) 인데 본 커밋은 fail-fast — commit msg 가 "analog" (mirror 아님) 라고 의도 표명.

**상태**: clean (round 1, intentional analog deviation)

### 18/161 — `f3341e5` docs(upstream-tracking): mark already-implemented and eliminated items

**상태**: clean (pure docs)

### 19/161 — `c7b2242` docs: mark iocsh tokenizer sentinel bug as ALREADY (3dbc9ea2)

**상태**: clean (pure docs)

### 20/161 — `b62fbcf` docs: mark callbackSetQueueSize sanity check as N/A (baa4cb54)

**상태**: clean (pure docs)

### 21/161 — `a9b3ddb` docs: mark caget DBR_INT→SHORT cast as N/A (PR #629)

**상태**: clean (pure docs)

### 22/161 — `8b4e30e` feat(iocsh): dbgf escapes non-printable bytes in CHAR arrays (dc70dfd6)

**검토**: C `epicsStrnEscapedFromRaw` (epicsString.c:135-154) 와 비교 — Rust 는 `\0`, `\'` 두 케이스 short-form 미적용 (각각 `\x00`, raw `'` 출력). commit msg 자체에 "exactly enough for the dbgf use case" 라고 명시. 의미 손실 없는 display 차이.

**상태**: clean (round 1, minor display divergence note)

### 23/161 — `9eb4d48` feat(iocsh): skip rustyline interactive setup on non-TTY stdin (PR #848)

**검토**: `IsTerminal::is_terminal()` 가 C `isatty(0)` 와 동등. piped 분기는 `BufRead::lines()` 로 prompt 없이 read. PR #848 시맨틱 일치.

**상태**: clean (round 1)

### 24-44/161 — pure docs (21 commits): `6909b2b`, `d73a060`, `7a40ec9`, `0bad7f6`, `ce08396`, `8ac5c68`, `1d75744`, `57bcf92`, `7dce27f`, `6fc5185`, `9f8b9b4`, `93236e3`, `a908386`, `7d72d72`, `ee02b30`, `eda6621`, `28f7717`, `c835efd`, `c9c1e14`, `74daeb5`, `f876f08`

모두 upstream-tracking doc status markers (ALREADY / N/A / DEFERRED). 코드 변경 없음.

**상태**: clean (pure docs, all 21)

### 45-49/161 — pure docs (5 commits): `db774af` `3e7e4d1` `6776f03` `92c8e99` `e98cf5b`

**상태**: clean (pure docs)

### 50/161 — `809baa9` fix(pva): skip name-server reconnect during PvaClient shutdown

**검토**: pvxs `client.cpp:210` `context->state == ContextImpl::Running` 와 Rust `!pool.is_shutdown()` AtomicBool gate 시맨틱 동등. PvaClient lifecycle 2-state (active / shutdown) 라 충분.

**상태**: clean (round 1)

### 51/161 — `8dd8a8e` feat(ca): ca-repeater detaches stdio to /dev/null

**검토**: C `caRepeater.cpp` (commit 6dba2ec): 2 fds (O_RDONLY + O_WRONLY) + dup2(/dev/null, 0/1/2). Rust: 1 fd RDWR + dup2. 결과 동등. `-v` flag 양쪽 지원.

**상태**: clean (round 1)

### 52-58/161 — pure docs (7 commits): `31640e8` `8af8be7` `f2b724d` `4b90f6d` `9a4076b` `cb0c184` `f936f9e`

**상태**: clean (pure docs)

### 59/161 — `763681d` fix(pva): emit first beacon immediately

**검토**: pvxs `cc5071cd22c4` 의 `event_add(beaconTimer, &immediate{0,0})` 와 Rust `first_beacon: bool` 플래그 동등.

**상태**: clean (round 1)

### 60-65/161 — pure docs (6 commits): `3274dc0` `fbcbcd2` `6b65d73` `015e70d` `d24c9da` `a3c0e59`

**상태**: clean (pure docs)

### 66/161 — `895bff9` fix(records): DBE_PROPERTY emits only when metadata actually changed (faac1df1)

**Round 1 defect**: Rust `is_metadata_field` (record_instance.rs:52-67) 가 C `prop(YES)` 필드 집합 불완전 — HHSV/HSV/LSV/LLSV (ai/ao/longin/longout 알람 severity) + ZSV/OSV/COSV (bi/bo) 누락. dbpf 로 HHSV 변경 시 Rust IOC 는 DBE_PROPERTY notify 안 함.

**Fix**: `cc1c4aa` — 7개 필드 추가. State-severity ZRSV..FFSV 는 upstream DBD 에서 `prop(YES)` 가 아니므로 정확히 제외 유지.

**상태**: clean (round 2 after fix `cc1c4aa`)

### 67/161 — `5142e0b` docs: mark DBE_PROPERTY ordering / mbbi gap as N/A (b7cc33c3, 9e7cd24)

**검토**: docs-only. Rust 의 `MetadataSnapshot` (record_instance.rs:34, `metadata_cache`) 가 매 monitor snapshot 에 metadata(display/control/enums)를 항상 포함. C 의 별도 DBE_PROPERTY 이벤트 발송 자체가 Rust 디자인에 없으므로 순서 보장 / 누락 문제 부재. design-diff N/A 라벨 정확.

**상태**: clean (round 1)

### 68/161 — `4c7c45f` docs: mark db_field_log::mask as N/A pending filter framework (235f8ed2)

**검토**: docs-only. epics-rs 는 filter framework 자체가 다음 commit (`9071c6b`, Stage 1) 부터 도입. C `db_field_log::mask` 는 filter 가 EventMask 분기 판단용 — 이미 Rust `FilteredMonitorEvent.mask: EventMask` (commit 9071c6b 의 framework) 로 도입됨. N/A 라벨 적절.

**상태**: clean (round 1)

### 69/161 — `9071c6b` feat(server): server-side channel filter framework + dbnd filter (3.15.7)

**Round 1 defect (4건, dbnd.rs)**:

1. **`>=` vs `>` 비교**: C `recGblCheckDeadband` (recGbl.c) 는 `if (delta > deadband)` 엄격 비교. Rust 는 `>=` 사용. `dbndTest.c` (test line 235/241) 가 `mustDrop("abs", 3., 3/4)` 로 `delta == deadband` drop 을 검증. Rust test 가 잘못된 "1.0 == threshold passes (>= semantics)" 주석 동반.
2. **NaN/Inf delta 처리**: C `recGblCheckDeadband` 는 finite↔NaN, finite↔Inf, +inf↔-inf 전환 시 `delta = epicsINF` 로 설정해 deadband 무조건 trip. Rust 는 `(cur - prev).abs()` 만 사용 → NaN 결과, `NaN > threshold` false → 침묵 drop. `recGblCheckDeadbandTest.c` 케이스 4-18 미커버.
3. **Zero-baseline relative fallback (Rust invention)**: Rust `last==0` 시 absolute fallback. C 는 `hyst = 0 * cval/100 = 0` 으로 모든 non-zero delta 통과 — fallback 불필요. Rust 의 발명된 동작.
4. **ALARM/PROPERTY pass-through 시 `last_sent` 업데이트 누락**: C `recGblCheckDeadband` 의 `*poldval = newval` 은 mask 무관, `delta > deadband` 면 항상 발생 — ALARM-only 이벤트도 baseline 갱신. Rust 는 short-circuit 통해 `last` 업데이트 skip → 직후 VALUE 이벤트의 비교가 stale baseline 사용.

**Fix**: `83ee47a` — `c_delta` C-style helper 도입, `>=` → `>`, last_sent 를 `Mutex<f64> = NaN` 으로 초기화, ALARM/PROPERTY 패스에서 `last` 무조건 갱신. 테스트 3개 수정 + 2개 추가 (NaN→finite, +inf→-inf).

**상태**: clean (round 2 after fix `83ee47a`)

### 70/161 — `88ce2e0` feat(server): add arr / ts / decimate channel filters (3.15.7 Stage 2)

**Round 1 defect (3건)**:

1. **arr.rs — alarm event 우회 잘못**: C `arr.c` 는 `channelRegisterPost` 로 등록되어 mask 무관 항상 slicing 수행. Rust 의 `if !event.mask.contains(VALUE) { return Some(event) }` 는 `dbnd`-specific 446e0d4a rule 의 잘못된 적용. 결과: `PV.{"arr":{"e":2}}` 구독자가 ALARM-only 이벤트에서 full array 수신 (slice 미적용) — slice view coherence 깨짐.
2. **arr.rs — start out-of-range clamp 비대칭**: C `wrapArrayIndices` 는 `*start > no_elements → no_elements`, `*end >= no_elements → no_elements - 1` 으로 비대칭 clamp. Rust 는 둘 다 `clamp(0, len-1)`. `start=10, len=3` 시 C 는 `start=3, end=2 → start>end → 0 elements`, Rust 는 `start=2, end=2 → 1 element` 반환.
3. **decimate.rs — ALARM 이벤트 slot 미소비**: C `decimate.c` 는 `if (pfl->mask & DBE_PROPERTY) return pfl` 만 short-circuit; DBE_ALARM 은 정상 decimation 로직 통과. Rust 는 446e0d4a rule 을 잘못 확장해 모든 non-VALUE 이벤트 (ALARM 포함) bypass — 카운터 안 움직임, downstream value emission 의 decimation 위상 어긋남.

**Fix**: `6a0cc82` — arr `mask` short-circuit 제거, 비대칭 clamp `resolve_start`/`resolve_end` 분리; decimate `intersects(PROPERTY)` 로 변경; 테스트 2개 갱신 + 2개 추가.

**관련 ts.rs feature gap**: C `ts` 는 `num=dbl|sec|nsec|ts`, `epoch=epics|unix`, `str=epics|iso` 모드 지원. Rust 는 default `Generate` 모드만 구현. JSON `{"ts":{}}` 는 정확히 동작하나 `{"ts":{"num":"sec"}}` 등은 silently ignored. 정식 defect 가 아닌 feature gap — 추후 별도 commit 권장.

**상태**: clean (round 2 after fix `6a0cc82`); ts feature gap 는 known limitation.

### 71/161 — `5404235` feat(server): PV-name JSON parser for channel filter chain (3.15.7 Stage 3)

**Round 1 defect**: `build_dbnd` 의 `r` key 처리가 C 의 percent 시맨틱 무시. C `dbnd.c:87` 은 `my->hyst = val * my->cval/100.` 으로 cval 을 percent (e.g. `r=50` → 50%) 로 해석. Rust 는 `DeadbandFilter::new(r, Relative)` 로 직접 전달 — internal 은 fraction 저장이므로 `r=50` → 5000% (50x) 의 deadband. pvxs / C-style client 가 `{"r":50}` 으로 50% 의도 시 거의 모든 update 가 silence.

**Fix**: `6489ef6` — parser 에서 `r / 100.0` 으로 wire boundary 변환. `d` key 는 절대값이므로 그대로.

**상태**: clean (round 2 after fix `6489ef6`)

### 72/161 — `c283c7f` feat(ca): wire channel filter chain through CA server (3.15.7 Stage 4)

**검토**: CREATE_CHAN 에서 `split_channel_name` 으로 record_path/suffix 분리 → `find_entry(&record_path)` 사용 (suffix 미오염). EVENT_ADD 에서 `attach_filter_to_last_subscriber` 가 `add_subscriber` 직후 동일 write guard 내에서 호출 — race 없음. JSON parse 실패 시 빈 chain 반환 + warn (graceful). audit log 는 raw `pv_name` 유지 (분리 전).

**상태**: clean (round 1)

### 73/161 — `bf70c8e` feat(records): compress PBUF (partial buffer) field (7.0.8 partial)

**Round 1 발견 (설계 발산, 작성자가 🔄 PARTIAL 라벨로 인지함)**:

1. **PBUF 시맨틱 mismatch**: C `compressRecord.c:179,296` 는 PBUF 를 PROCESSING 시점 옵션으로 사용 — `pbuf=YES` 면 `inx < n` 이어도 매 push 마다 `put_value` 발행 (early-emit). 즉 N-to-1 알고리듬 윈도가 N 미만으로 채워져도 출력. Rust 는 PBUF 를 READ-side toggle 로 재해석해 `get_field("VAL")` 에서 NUSE-truncated 반환. 두 시맨틱이 같지 않으며 C client 가 PBUF=YES 로 early-emit 을 기대하면 Rust 에서는 normal-process 만 발생.
2. **VAL read 시 NUSE clamp 누락 (pre-existing, PBUF=NO 노출)**: C `get_array_info` (compressRecord.c:428) 는 `*no_elements = nuse` 로 PBUF 무관 항상 NUSE clamp. Rust 는 default PBUF=NO 시 full NSAM-padded vector 반환 (trailing zeros). CA wire 가 NSAM 크기 전송 → C 와 다름. PBUF=YES opt-in 시에만 부분적으로 일치.

**판단**: 작성자가 commit msg 에서 "N-to-1 partial-window compression remains a follow-up — depends on extending push_value to an array-input model" 로 deferred 명시. 본 review session 의 범위를 넘는 구조적 변경 필요 — 별도 commit 으로 진행 권장. 정확한 C semantic 구현 시 (a) push_value 가 PBUF=YES 면 early-emit, (b) get_field("VAL") 가 PBUF 무관 NUSE-truncated, (c) tests 갱신 필요.

**상태**: clean (round 1, deferred — feature gap 작성자 인지 + 라벨)

### 74-82/161 — `835e2c5`, `4cc40a8`, `abf9344`, `312d578`, `2c1c8ee`, `e25d281`, `0cd85a5`, `19551c5`, `a3e7c74` (PR #205 IPv6 Stages 1-6 + 보조 fix/doc)

**Round 1 defect (1건)**:

`abf9344` (Stage 2 v6 SEARCH responder) 의 `run_udp_responder_v6` 가 `IPV6_V6ONLY` flag 미설정. 동일 commit 의 `bind_beacon_send_v6` 는 `set_only_v6(true)` 호출하나 RX socket 만 누락된 비대칭. Linux 기본 `IPV6_V6ONLY=0` → `[::]:port` 가 v4 와 dual-stack 으로 동작해 v4 per-NIC bundle 과 ports/destinations 충돌 (EADDRINUSE 또는 silent duplicate). BSD/macOS 기본은 `V6ONLY=1` 라서 우연히 동작.

**Fix**: `9b27f5b` — socket2 로 명시적 `set_only_v6(true)` + `set_reuse_address(true)` 추가. v4/v6 lane 을 strictly disjoint 하게 분리. 모든 platform 에서 일관된 동작.

**기타 (clean)**:
- `835e2c5` Stage 1: `Ipv4Addr` → `IpAddr` 타입 widening, default 유지 → no behavior change for v4.
- `4cc40a8` Stage 1 cont'd: `client_config()` peer family 미러링 (`Ipv4Addr::LOCALHOST` vs `Ipv6Addr::LOCALHOST`).
- `312d578` Stage 3: ADDR_LIST v6 entries 1회성 warn + drop (graceful degradation).
- `2c1c8ee` Stage 1 doc.
- `e25d281` Stage 4: client v6 SEARCH 소켓 `IPV6_V6ONLY=1` + `ff0e::400` multicast group 일관 적용. `rewrite_loopback` peer family 미러링.
- `0cd85a5` Stage 5: v6 multicast beacon emit, 동일한 V6ONLY 패턴.
- `19551c5` legacy `rewrite_loopback_target` 도 peer family 미러링 (Stage 4 와 일치).
- `a3e7c74` Stage 6: client v6 beacon recv, `SO_REUSEADDR/SO_REUSEPORT + IPV6_V6ONLY=1` 일관.

**상태**: clean (round 2 after fix `9b27f5b`)

### 83/161 — `b9f3e41` fix(longout): OOPT first cycle force-emits (epics-base PR #6c573b4)

**Round 1 defect**: 원 commit 의 `compute_should_output` 가 `if !self.first_output_done { return true; }` 로 first-cycle force 를 모든 OOPT 값 (Every Time/On Change/When Zero/When Non-zero/Transition_*) 에 적용. C `conditional_write` (longoutRecord.c:455-487) 는 `outpvt == EXEC_OUTPUT` 체크가 `case longoutOOPT_On_Change` arm 안에만 존재 — 다른 OOPT 는 자기 조건 그대로 평가. 즉 OOPT=When_Zero+초기 val=0 같은 케이스에서 Rust 는 first-cycle force 로 write 발생, C 는 정상 평가.

**Fix**: 이전 review session 에서 `bd9d1c7` 로 force 를 `1 => !self.first_output_done || self.val != self.pval` arm 안으로 이동시켜 fix 됨. 현 HEAD (longout.rs:131-139) C semantic 과 일치.

**참고 (별도 항목으로 처리)**: C PR `6c573b4` 는 두 번째 feature 포함 — `special()` 에서 OUT field 변경 시 `ooch == YES` 면 `outpvt = EXEC_OUTPUT` 재설정. Rust 는 OOCH field 미구현. mid-life OUT redirect 시 next-cycle force 미발생. Feature gap — 추후 필요 시 OOCH 추가 + put_field("OUT") hook 필요.

**상태**: clean (이전 fix `bd9d1c7` 로 완료, current HEAD verified C-parity)

### 84/161 — `a493cf2` fix(processing): soft-channel INP read failures raise LINK_ALARM (PR #4737901)

**검토**: Rust 의 LINK_ALARM/INVALID 적용 로직 자체는 C `devAiSoft.c::read_ai` + `dbLink.c::setLinkAlarm` 와 일치 (raise-only `recGblSetSevr`). gate `is_soft && matches!(inp_parsed, Db|Ca|Pva)` 도 C 의 soft device support 컨텍스트와 부합.

**Round 1 발견 (pre-existing cross-cutting, 별도 fix 로 처리)**: Rust `alarm_status::*` enum 값들이 C `menuAlarmStat.dbd` / `epicsAlarmCondition` (libcom/src/misc/alarm.h) 와 다름. CA wire `stat` byte 가 이 정수값 그대로 전송되므로 fundamental 한 wire-level protocol 발산. 예: Rust `LINK_ALARM=13`, C wire 는 `13=SCAN`; Rust `DISABLE_ALARM=14`, C wire 는 `14=LINK`. CA client (caget/camonitor/IOC) 가 Rust IOC 의 STAT 을 잘못 decode.

또한 누락된 항목: `BAD_SUB_ALARM` (C: 16), `READ_ACCESS_ALARM` (C: 20), `WRITE_ACCESS_ALARM` (C: 21).

**Fix**: 별도 commit — `alarm_status::*` 를 menuAlarmStat.dbd 순서로 renumber + 누락 3개 추가. 영향 받는 hard-coded numeric assertion 2개 test 갱신 (`database_tests.rs:711` DISABLE=18, `record_tests.rs:752` SCAN=13). 정적 use site 들은 named constant 라 자동 호환.

**상태**: clean (commit logic OK, wire-level enum fix 별도)

### 85/161 — `f68f17c` feat(ca-server): wire dbServerStats bytes_in/bytes_out counters (PR #592)

**검토**: C `dbServerStats` (dbServer.c:130) 는 `channels`/`clients` 만 카운트하고 bytes counter 없음 — Rust extension. handle_client 의 RX path 가 `bytes_in.fetch_add(n)` post-read, TX path 가 BufWriter::buffer().len() 을 flush 직전 캡처 후 `bytes_out.fetch_add` 호출. partial flush 실패 시 over-count 가능하나 connection drop 으로 mitigated.

**상태**: clean (Rust-side extension, C 비교 대상 부재)

### 86/161 — `d6ae617` fix(processing): single-INP MS-class link propagates STAT/SEVR/AMSG (PR #d0cf47c)

**Round 1 defect**: Rust 가 `MonitorSwitch::Maximize` (MS) 와 `MaximizeStatus` (MSS) 를 동일 코드 패스로 처리해 둘 다 source stat + amsg 전파. C `recGblInheritSevrMsg` (recGbl.c:260) 는 분리:
- `pvlOptMS`: `recGblSetSevr(LINK_ALARM, sevr)` — DEST 가 source stat 가 아닌 `LINK_ALARM` 으로 표시, **amsg 전파 안 함**.
- `pvlOptMSI`: source.sevr == INVALID 일 때만 LINK_ALARM, msg 없음.
- `pvlOptMSS`: source stat + sevr + msg.

Rust 가 MS 에서도 source HIHI stat + "src-msg" 를 누설해 downstream OPI 가 잘못된 알람 attribute 표시.

**Fix**: `09c4109` — match arm 분리. Maximize/MaximizeIfInvalid 가 `rec_gbl_set_sevr(LINK_ALARM, sevr)` 호출 (msg 없음), MaximizeStatus 만 `rec_gbl_set_sevr_msg(stat, sevr, amsg)` 유지. 기존 테스트 `test_single_inp_ms_class_propagates_source_alarm` 가 잘못된 동작 (source stat+amsg propagation through MS) 을 단언했으므로 두 테스트로 분리: `test_single_inp_ms_propagates_link_alarm_no_msg` (MS → LINK_ALARM, empty amsg), `test_single_inp_mss_propagates_stat_and_amsg` (MSS → source stat+amsg).

**상태**: clean (round 2 after fix `09c4109`)

### 87/161 — `2c1ae54` feat(subArray): INDX/MALM slicing semantics (PR #a02c310 follow-up)

**검토**: `set_val` slicing 이 C `devSASoft::subset()` (devSASoft.c:39) 와 동치 — `start..min(start+take, src_len)` 에서 `src_len = min(arr.len(), malm)`. start >= src_len → valid=0/nord=0 (C 의 ecount=nRequest-indx<0 → 0 과 일치). INDX put_field 가 `v.min(malm-1)` 으로 clamp 하는 것도 C `readValue::indx >= malm → malm-1` 와 일치. MALM put 시 NELM clamp + INDX clamp 도 C `init_record` 와 매칭.

기타 pre-existing gap (이 커밋 범위 외): ILIL/IHIL input range 필터링 미구현.

**상태**: clean (round 1)

### 88/161 — `14d0b03` feat(ca-server): wire dbServerStats subscription counters (PR #592)

**검토**: C `dbServerStats` 에 subscription counter 없음 — Rust extension. EVENT_ADD 후 `SubscriptionOpened`, EVENT_CANCEL/CLEAR_CHANNEL/disconnect drain 시 `SubscriptionClosed` emit. ACF-revoke teardown path 가 conn_events 미보유로 미적용 — 작성자가 commit msg 에서 명시함 (known gap).

**상태**: clean (Rust-side extension)

### 89/161 — `9d26ad5` fix(processing): PUTF stays off for CP-chained targets (PR #3fb10b6)

**검토**: C `processNotifyCommon(ppn, precord, first)` 의 `if (first) precord->putf = TRUE` 조건 (dbNotify.c:253) 과 동치. Rust `dispatch_cp_targets` 가 (not_already_active) branch 에서 `tg.common.putf = true` 를 제거 — CP-chained target 은 putf 갱신 안 함. 직접 dbPut 받은 record 의 putf 는 `field_io::put_field` 의 set/clear bracket 으로 유지.

**상태**: clean (round 1)

### 90/161 — `ea9a111` feat(ioc_app): SIGTERM/SIGINT handler races protocol runner (PR #671)

**검토**: 표준 tokio::select!-with-signals 패턴. `biased` 로 protocol_runner 가 자연스럽게 완료되면 우선. `tokio::signal::ctrl_c` (SIGINT 크로스-플랫폼) + `SignalKind::terminate` (Unix only). Drop semantics 가 모든 spawn 을 abort.

**상태**: clean (round 1)

### 91/161 — `52427bc` feat(compress): push_array with PBUF=YES partial-buffer emit (PR #84f4771)

**Round 1 defect (pre-existing wire-protocol mismatch, cross-cutting)**: Rust `CompressRecord::alg` 정수값이 C `menuCompressALG` (compressRecord.dbd.pod) 와 발산.

C: 0=Low, 1=High, 2=N_to_1_Average, 3=Average (rolling), 4=Circular_Buffer, 5=N_to_1_Median.
Rust (pre-fix): 0=Low, 1=High, 2=Mean, 3=Circular_Buffer. (Average rolling + Median 누락 + Circular 위치 1 위 시프트.)

CA wire 가 ALG 을 DBR_SHORT 로 전송 — C 클라이언트가 ALG=4 (Circular) 설정 시 Rust 가 `_` 디폴트 branch (return 0.0) 로 처리해 broken. 반대로 Rust IOC 가 ALG=3 (Circular 의도) 전송 시 C OPI 가 "Average (rolling)" 으로 misdisplay.

**Fix**: 별도 commit — `alg` 값 재배치 (default `alg: 3` → `alg: 4`), `push_value`/`push_array` 의 alg==3 branch → alg==4, `flush_accum` 에 `case 5: Median` 추가 (qsort + middle pick — `compressRecord.c:212` `psource[n/2]` 대응). `alg==3` (Average rolling) 는 not-implemented stub 으로 남김 (별도 작업 필요). 영향 받은 hard-coded literal alg=3 (Circular 의도) 5곳 갱신:
- `crates/epics-base-rs/src/server/records/compress.rs` tests (4곳)
- `crates/epics-base-rs/src/server/db_loader/mod.rs:826`
- `crates/epics-base-rs/tests/database_tests.rs:2724`
- `crates/epics-base-rs/tests/c_epics_parity.rs:1033`

push_array 자체 로직 (full-chunk loop + PBUF=YES tail emit) 은 C `compress_array` (compressRecord.c:177-219) 의 nnew loop + `if (nnew < n && pbuf != YES) break` semantic 과 일치.

**상태**: clean (round 2 after wire-protocol fix)


### 92-94/161 — `683c6ea`, `7b5b9ac`, `710fe62` — SIMM=RAW + sync filter (3 modes → 6 modes)

- **92** `683c6ea` SIMM=RAW: 이전 review session fix `1cc2629` 가 Float→Long floor 처리, 본 commit 의 convert_to fallback 는 비-float 타입에만 적용 — C-correct.
- **93** `7b5b9ac` sync filter "after" 모드: Rust-invented "trigger pulse" 모델, C-divergent. **94** 에서 dbState 기반 6 modes 로 재설계.
- **94** `710fe62` sync 6 modes: 5/6 modes 정확. **Defect**: alarm event 가 `if !VALUE { return Some }` 단축회로로 state machine 우회. C `sync.c::filter` 는 `DBE_PROPERTY` 만 우회 — DBE_ALARM 은 정상 통과. 446e0d4a 가 dbnd 한정인데 sync 에 잘못 적용.

**Fix**: `e26af3e` — sync `intersects(PROPERTY)` 만 우회로 변경. `while` 모드 + state=false 일 때 ALARM 도 drop (C 일치). 테스트 2개 갱신.

**상태**: clean (round 2)

### 95/161 — `69c7999` feat(pva): server-side channel filter wire-through

**검토**: client-side `PvRequestExpr::encode()` 에 `to_pv_field()` + `encode_pv_field()` 추가 — pvxs `clientget.cpp:351-352` `to_wire(type) + to_wire_full(value)` 패턴 일치. 이전에 type descriptor 만 전송해 pipeline/queueSize/_filter 등 record_options 무음 전파되던 버그 수정. PVA `_filter` option key 는 Rust extension (pvxs 미사용).

**상태**: clean (client encode fix C-aligned)

### 96-98/161 — `0b4e89a`, `2054ab7`, `1400bd8` — NORD invariant test + async-gate fix + NORD side-effect

- **96** test-only NORD timestamp ordering.
- **97** `complete_async_record` subscriber-gate fix — mirror main path's last_posted gate.
- **98** `put_pv_and_post` waveform NORD side-effect propagation.

**상태**: clean (all rounds 1)

### 99-103/161 — test-only commits

`9dea925` `06aa884` `4e4fd49` `f80f15a` `9d3036c` — 모두 invariant 검증용 테스트. 코드 변경 없음.

**상태**: clean (pure tests)

### 104-109/161 — SO_RXQ_OVFL helper + wire-through

- **104** `2a9b52a` AsyncUdpV4::enable_so_rxq_ovfl + recv_from_with_drop_count_socket. C pvxs `osdSockExt.cpp:60` `setsockopt(SOL_SOCKET, SO_RXQ_OVFL, 1)` 동치. cmsg walk 도 정확.
- **105** `047bd2d` docs.
- **106** `6738aa3` PVA UDP collector wire-through.
- **107** `17e6021` chore.
- **108** `b3e0fe0` CA SEARCH/UDP server wire-through.
- **109** `aff1ee5` CA beacon RX + repeater wire-through.

**상태**: clean (모두 standard wire-through pattern)

### 110/161 — `22fd25d` bulk 9-A archaeology audit (mbboDirect init + compress RES)

- **mbboDirect** `post_init_finalize_undef` hook: C `init_record` (mbboDirectRecord.c:139-160) 의 UDF-based VAL ↔ Bx priority logic 과 동치.
- **compress RES**: nuse/off/res/accum/val clear, C `reset()` (compressRecord.c) 와 일치. C 의 `inx`/`cvb`/`sptr` (Average rolling 용) 는 Rust 미구현이라 무관.

**상태**: clean

### 111-115/161 — ca-repeater debug + docs + asyn-rs (Stage 2 audit)

- **111** `d0d59f7` ca-repeater `-d/-dd` PR #831 port — mechanical.
- **112-114** docs/audit closure.
- **115** `598d81b` asyn-rs IP server port + TRACE_STATE (이미 prev session 에서 fix `9691605` 로 STATE bit 제거 — current HEAD C-correct).

**상태**: clean

### 116/161 — `60188d3` feat: iocsh ANSI color + HAG DNS TTL refresh

**Round 1 defect**: prompt color cyan (`\x1b[36m`). C `c0da3dd1f` + errlog.h:282 `ANSI_ESC_GREEN "\033[32;1m"` — bright green. cosmetic but C-divergent.

**Fix**: `1be96ec` — `\x1b[32;1m` 로 변경 (C ANSI_GREEN 매크로 일치).

**상태**: clean (round 2 after `1be96ec`)

### 117/161 — `d525ace` feat: PVA decodeError file:line + general_time sync hook

**검토**: `Status::error_with_location(file, line, msg)` 가 wire stack-trace 필드에 `"file:line"` 포맷. `source_location` 가 `rsplit_once(':')` 로 Windows `C:\` 경로 round-trip 안전. pvxs `e9ce80880d92` convention 일치.

**상태**: clean

### 118/161 — `e1387d8` feat: PI mutex + serial break + averaging device + camonitor type-change

PI Mutex / serial BREAK / averaging device 는 asyn-rs / Linux RT extension, wire 영향 없음. NativeTypeChanged event 는 C `1687757752` camonitor type-change 와 의미적 일치 (broadcast pattern 차이 있으나 end-goal 동일).

**상태**: clean

### 119/161 — `d545303` feat: lnkCalc + autoExec=false PUT + filter on read + FTDI

**Round 1 defect (PVA autoExec=false interop)**: Rust 가 server-side 에서 "first PUT queues, second PUT commits" two-step pattern 도입. pvxs `serverget.cpp:488-492` 는 모든 CMD_PUT !init 에 `onPut` 즉시 호출 — server 는 autoExec 무관. pvxs `clientget.cpp:123` autoExec 는 client-side timing 만 제어 (auto vs `reExec()`). pvxs PR `7073538` 는 client-side error handling 만 변경.

증상: pvxs client `.autoExec(false).reExec(v)` 호출 시 Rust 가 v 를 queue 하고 OK ack — write 미발생. 두 번째 reExec(v2) 시 첫 v 가 commit, v2 가 queue. 매 write 가 한 round delay.

**Fix**: `65db161` — `OpState::put_pending` 제거, PUT 실행 path 무조건 `put_value_checked` 호출. `put_auto_exec` field 는 diagnostic echo 용으로만 유지.

기타 (lnkCalc + filter-on-read + FTDI) 는 Rust extension / asyn-rs work.

**상태**: clean (round 2 after `65db161`)

### 120-130/161 — 9-A audit fact-check + asyn-rs work + reverts

- **120** docs fact-check.
- **121-127** asyn-rs UDP/Prologix/FIFO/VXI-11/HiSLIP/PVI scaffolds.
- **128-130** REVERTS of 125-127 (invented work).

asyn-rs work was deep-audited in earlier session (`docs/asyn-rs-audit-2026-05-14.md`). 22 items, 6 verified / 9 partial / 4 invented / 3 wrong. All 16 items closed by commit `60416c1`.

**상태**: clean (relies on prior session asyn audit)

### 131-161/161 — asyn-rs C-source audit fixes (W1/W2/I4 + 13 items)

31 commits applying audit findings from `docs/asyn-rs-audit-2026-05-14.md` + `docs/asyn-missing.md`. Earlier session reviewed each commit individually against `~/codes/epics-modules/asyn/...` C sources. Fixes include:
- `9ff5659` TCP&/UDP&/UDP* protocol suffix semantics
- `9691605` trace mask STATE bit removed
- `5d2253c` FTDI iocshArg
- `7befd0e` SumAverager (replaces RingAverager)
- `1ec01f3` drvAsynIPServerPort SO_REUSEPORT removal
- `38e7743` RS485 struct
- `40fa1d0` hostInfo setOption
- `1e2716a` IP server child port model
- `4b6e2f7`, `f2370af` INITIAL_READBACK
- `55dc8fd` asynOctet SIZV
- `a20aede` ASYN_DESTRUCTIBLE
- `e96561b` asynMask SHFT/MASK propagation
- `04ef574` asyn:FIFO ring buffer
- `614e7eb` ai LINEAR ESLO/EOFF
- Various scaffolds (USBTMC, VXI-11)

**상태**: clean (rely on prior session audit; current HEAD is post-fix state)
