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

