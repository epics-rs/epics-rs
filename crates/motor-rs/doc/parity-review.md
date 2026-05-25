# motor-rs Parity Review — epics-modules/motor 전수 비교

분석일: 2026-05-16
대상: `motor-rs` (~/codes/epics-rs/crates/motor-rs)
upstream: `epics-modules/motor` HEAD `f3d089ba` (2026-05-15)

대상 범위: motorRecord.cc/.dbd/.html + asynMotorController/Axis + devMotorAsyn + motordrvCom.
범위 밖: 개별 드라이버 서브모듈(modules/motorAcs, motorParker 등 36개 PR).

데이터 수집:
- 머지된 PR 110개 (`gh pr list --state merged --limit 300`)
- 모든 issue 106개 (`gh issue list --state all --limit 400`) — OPEN 11
- motorRecord.cc 151 commits, motorRecord.dbd 42 commits, asynMotorController.cpp 48 commits, asynMotorAxis.cpp 27 commits
- 닫힌 unmerged PR 31개, OPEN PR 1개

---

## ⓘ 구현 상태 (2026-05-16 갱신)

이 보고서의 §9 Sprint 1~5 및 §4 동작 항목, §6 OPEN issue가 **모두 구현 완료**되었다.
테스트는 139개 → 233개로 증가, workspace 전체 빌드·clippy 통과.

| 그룹 | 항목 | 상태 |
|---|---|---|
| Sprint 1 | ACCS/ACCU, RSTM, RHLM/RLLM, SYNC | ✅ 구현 |
| Sprint 2 | MIP_EXTERNAL, DLLM>DHLM LVIO, URIP error STOP, VAL+HOMF clear, jog driver-stop clear, LVIO jog reject, home soft-limit skip, ACCL@VELO==VBAS | ✅ 구현 |
| Sprint 3 | motorActVelocity(RVEL), enablePCO+PCO PV, moveToHome, VBAS_UNSUPPORTED bit | ✅ 구현 |
| Wire 모델 | driver-bound acceleration을 EGU/sec²로 통일 (C `accEGUfromVelo`) | ✅ 구현 |
| RSTM 통합 | `initial_readback`의 RSTM restore 분기 + #196 MRES-mismatch 인터록 | ✅ 구현 |
| §4 검증 | 4.2 RA_PROBLEM no auto-stop, 4.3 DLY+STOP "DELAY wins", 4.9 음수 BDST relative, 4.14 FLNK=DMOV transition, 4.16 RDBD validation | ✅ 검증/수정 |
| OPEN issue | #170 JOGF+JOGR latest-wins, #196 MRES mismatch, #231 LOAD_POS block(`LOADPOS_BLOCK` PV), #192 raw 필드 i64 | ✅ 구현 |

아래 본문(§3~§10)은 분석 시점(구현 전) 기준의 갭 분석으로, 작업 근거 기록으로 보존한다.

---

## 0. TL;DR

motor-rs는 **2018년경(약 R6.11~R7.0) 시점의 motorRecord** 기능 집합에 가깝다. 다음이 사실:

**잘 구현된 영역:**
- 좌표 변환 (dial↔user↔raw, MRES/ERES sign 포함)
- 9-phase motion state machine (Idle/MainMove/BacklashFinal/Retry/Jog/JogStopping/JogBacklash/Homing/DelayWait)
- Retry 4 modes (Default, Arithmetic, Geometric, **InPosition** 포함 — `85511023` 2013 반영)
- Backlash 2-stage (BDST/BVEL/BACC/FRAC), SET/FOFF/DIR, NTM/NTMF (`#521fcb1e`/`#8c6e56ed`), SPDB (`#0ea2d9ec` 2018)
- MSTA/MIP 비트 wire-호환 (15/16비트)
- SPMG, DLY, STUP, ADEL/MDEL, IGSET, auto_power on/off delay (#36dfab4a 2015 대응)
- Profile move 기본 API (initialize/build/execute/abort/readback)
- AxisRuntime (single tokio task per axis) — v0.2 신규 아키텍처

**핵심 누락 — DBD parity 깨짐 (P1):**
| 필드 | 도입 | upstream 출처 |
|---|---|---|
| **ACCS / ACCU** | 2018-12 | `#36177f7b` / PR #122 / PR #203 |
| **RSTM** | 2020-06 | `#2906f3d8` / PR #160 |
| **RHLM / RLLM** (PV) | 2022-11 | `#2e89b552` / PR #193 / `#fd808eb2`(MRES<0) |
| **SYNC** | 2010-04 | `#82c26005` |

**핵심 누락 — base class API (P2):**
- **motorActVelocity_** asyn parameter (`#314ef89a` 2026 / PR #238) — RVEL 분리
- **Position Compare**: `enablePCO(bool)` + `PCO_*` 5개 파라미터 (`#05b25c1d` 2026 / PR #248)
- **MIP_EXTERNAL** flag — 외부 시작 move 감지 (`#ea063f5f` 2008)
- **moveToHome framework** (`#a6f64591` 2011 / `#5f421e9a`)

**미해결 OPEN issue (P3 — 정책 결정):**
- #170 JOGR+JOGF 동시 활성
- #196 autosaved MRES mismatch → 잘못된 위치 복원
- #231 LOAD_POS 실패 시 DVAL/OFF 불일치
- #192 RRBV/REP/RMP 32-bit (Rust는 자연스럽게 i64이지만 PV 노출 시 결정 필요)
- #76 VBAS-not-supported MSTA bit
- #230 Model 3 PCO API

종합: motor-rs는 핵심 motion semantics는 매우 견고하나, **2019~2026 사이에 upstream에 추가된 5개 필드(ACCS/ACCU/RSTM/RHLM/RLLM/SYNC) 와 2026 신규 기능(motorActVelocity, PCO) 이 모두 누락**. 또한 `MIP_EXTERNAL` 부재는 외부 시작 move를 record가 인식하지 못한다는 의미로, 실제 사용 환경에서 회귀 가능성이 있다.

---

## 1. motor-rs 현재 구현 범위

### 1.1 필드 (74개, src/fields.rs + src/record/field_access.rs)

| 카테고리 | 필드 |
|---|---|
| 위치 | VAL, RBV, RLV, OFF, DIFF, RDIF, DVAL, DRBV, RVAL, RRBV, RMP, REP |
| 좌표 변환 | DIR, FOFF, SET, **IGSET**, MRES, ERES, SREV, UREV, UEIP, URIP, RRES, RDBL |
| 속도/가속 | VELO, VBAS, VMAX, S, SBAS, SMAX, ACCL, BVEL, BACC, HVEL, JVEL, JAR, SBAK |
| Retry | BDST, FRAC, RDBD, **SPDB**, RTRY, RMOD, RCNT, MISS |
| 제한 | HLM, LLM, DHLM, DLLM, LVIO, HLS, LLS, HLSV |
| 제어 | SPMG, STOP, HOMF, HOMR, JOGF, JOGR, TWF, TWR, TWV, CNEN |
| 상태 | DMOV, MOVN, MSTA, MIP, CDIR, TDIR, ATHM, STUP |
| PID | PCOF, ICOF, DCOF |
| 디스플레이 | EGU, PREC, ADEL, MDEL |
| 타이밍 | DLY, NTM, NTMF |

### 1.2 Motion phase enum (src/flags.rs)

```
Idle, MainMove, BacklashFinal, Retry, Jog, JogStopping, JogBacklash, Homing, DelayWait
```

(README는 `BacklashApproach`라 적었지만 실제 코드에는 없음. BacklashFinal로 단일화되어 있고 `DelayWait`이 추가됨. **README 수정 필요**.)

### 1.3 Process 흐름

```
process() → determine_event() → do_process()
  ├─ STUP fast path
  ├─ Sub-step pulse recovery
  ├─ UserWrite (+ 동시 DeviceUpdate apply)
  ├─ plan_motion(CommandSource)
  └─ check_completion()
       ├─ DELAY_ACK / driver done / STOP wait
       └─ phase별 finish (MainMove→Backlash/Retry, etc.)
```

`record/` 디렉토리: `mod.rs`, `state_machine.rs`, `command_planner.rs`, `field_access.rs`, `status_update.rs`.

### 1.4 Device support 메서드 (src/device_support.rs)

`MoveAbsolute`, `MoveRelative`, `MoveVelocity`, `Home`, `Stop`, `SetPosition`, `SetClosedLoop`, `DeferMoves`, `ProfileInitialize/Build/Execute/Abort/Readback`.

### 1.5 MIP/MSTA 비트

**MipFlags(u16):** JOGF, JOGR, JOG_BL1, HOMF, HOMR, MOVE, RETRY, LOAD_P, MOVE_BL, STOP, DELAY_REQ, DELAY_ACK, JOG_REQ, JOG_STOP, JOG_BL2, EXTERNAL(0x8000).

> **EXTERNAL 비트는 정의되어 있으나 (flags.rs:36) `MIP_EXTERNAL` 설정 로직이 없다.** `rg -n 'MipFlags::EXTERNAL' src/`는 정의 외 어디서도 set/check하지 않는다. C 코드 `#ea063f5f`(2008)의 `movn && dmov → dmov=false, MIP=EXTERNAL, pp=true` 분기가 없다. → §4의 P0 항목.

**MstaFlags(u32, 비트 위치 C와 동일):** DIRECTION, DONE, PLUS_LS, HOME_LS, SLIP, POSITION, SLIP_STALL, EA_HOME, ENCODER_PRESENT, PROBLEM, MOVING, GAIN_SUPPORT, COMM_ERR, MINUS_LS, HOMED. 15비트 모두 정의됨.

---

## 2. Upstream 변경 분류 — 최종 요약

| 카테고리 | 머지 PR | 비고 |
|---|---|---|
| A. motorRecord 동작 변경 | 22 (+housekeeping 7) | parity 핵심 |
| B. asynMotor base / devMotorAsyn / motordrvCom | 15 | parity 핵심 |
| C. 신규 필드/메뉴 | A 내 7건 | SPDB/ACCS/ACCU/RSTM/RHLM/RLLM/IGSET |
| D. 버그 수정 | A/B 내 16건 | 회귀 테스트 대상 |
| E. motorRecord 문서만 | 8 | |
| F. CI/빌드/라이선스 | 10 | |
| G. 드라이버 서브모듈 | 36 | 범위 밖 |
| H. IOC 예제/Makefile | 5 | |

OPEN PR 1개 (#211 SoftMotor MINP). Unmerged closed 31개 — 그 중 motorRecord 영향은 9개(§5).

---

## 3. Parity 매핑표 — 신규 필드 / 신규 메뉴 (P1)

| 필드/메뉴 | upstream commit / PR | 도입일 | motor-rs 상태 | 영향 |
|---|---|---|---|---|
| **SPDB** | `0ea2d9ec` / #114 | 2018-11 | ✅ 있음 (`rec.retry.spdb`, `parity_retry.rs`로 검증) | — |
| **ACCS** | `36177f7b` / #122 | 2018-12 | ❌ **없음** | ACCL과 mutex 의미; DB load 시 ACCS!=0이면 우선 사용 (`7b87f3b9`) |
| **ACCU** (menu motorACCSused: Accl/Accs) | `36177f7b` + `63bfe5d0`(readback→control) / #203 | 2018-12 → 2023-05 | ❌ **없음** | autosave 대상으로 변경됨; VBAS/SBAS 변경 시 ACCU에 따라 ACCS/ACCL 갱신 |
| **RSTM** (menu: Never/Always/NearZero/Conditional) | `2906f3d8` / #160 | 2020-06 | ❌ **없음** | `#150`(2015)의 자동 incremental restore를 partial revert. devMotorAsyn `init_controller`에 RSTM 분기. autosave 시점 동작 결정 |
| **RHLM / RLLM** (raw soft limits, PV) | `2e89b552` / #193 + `99d0c414` post fix + `fd808eb2`(MRES<0) / #206 | 2022-11 → 2023-05 | ⚠️ **부분** — 구조에 `rhlm/rllm` 있으나 PV 미노출, MRES 변경 시 자동 재계산 없음 | MRES 변경이 user/dial limit을 깨던 #191 해결책. Rust 측은 PV 노출 + post 필요 |
| **SYNC** | `82c26005` | 2010-04 | ❌ **없음** | VAL/DVAL/RVAL ← RBV/DRBV/RRBV 단일 트리거 |
| **IGSET** | `5cad1053` / #53 | 2017-01 | ✅ 있음 (`rec.conv.igset`, `field_access.rs:829`) | — |
| **NTMF** | `8c6e56ed`/`95143109` | 2008-02 | ✅ 있음 | — |
| **NTM** | `521fcb1e` | 2002-10 | ✅ 있음 + parity_ntm.rs | — |
| **RMOD-I** (In-Position) | `85511023` | 2013-06 | ✅ 있음 (4 modes — Default/Arithmetic/Geometric/InPosition) | — |
| **HVEL** | `4ee2b783` | 2003-05 | ✅ 있음 | — |
| **JVEL / JAR** | `4d2e1745` | 2001-05 | ✅ 있음 (JAR 명시 확인 필요) | — |
| **STUP** | `025f2328` | 2003-12 | ✅ 있음 | — |
| **ADEL / MDEL** | `df1586fd` | 2010-03 | ✅ 있음 | — |
| **CDIR**(←PDIF) | `0423811d` | 2001-10 | ✅ 있음 | — |
| **RES 폐기** | `042ca618` | 2010-03 | ✅ 없음 (자연) | — |
| **`prop(YES)` 마킹** | `b77c331f` / #202 / #204 | 2023-05 | N/A (Rust측 PV 노출 모델 결정 필요) | CA dbe_property monitor 대상 |

---

## 4. Parity 매핑표 — 동작 변경 (P0 — 회귀 위험)

다음은 **motor-rs가 동작상 다르거나 결손**일 가능성이 있는 항목. 각 항목은 검증을 위해 회귀 테스트 추가가 필요.

### 4.1 외부 시작 move 인식 (MIP_EXTERNAL) ❌

- upstream: `ea063f5f` (2008-04). 컨트롤러가 driver-측에서 직접 motion을 시작하면 `movn=true && dmov=true`가 들어옴. record는 이를 외부 move로 인식하여 `dmov=false`, `MIP=EXTERNAL`, `pp=true` 설정.
- motor-rs: `MipFlags::EXTERNAL = 0x8000`는 정의되어 있지만 어디서도 set/check되지 않음. 외부 move 발생 시 record가 인식하지 못해 NTM/post-process가 동작하지 않을 가능성.
- **조치:** `status_update.rs` (또는 `state_machine.rs`)에서 `motor_status.moving && dmov`인데 phase==Idle이면 EXTERNAL set + dmov clear + pp 트리거.

### 4.2 RA_PROBLEM 자동 stop 정책 ⚠️ 검증 필요

- upstream history: `303a9208`(2014) RA_PROBLEM → stop=true → `95c0a4ca`(2018, PR #109) 부분 revert (driver 책임으로 환원, issue #25/#100).
- motor-rs: `PROBLEM` MSTA bit 정의는 있고 (`flags.rs:40`, `status_update.rs:187`) 자동 stop 발행은 없는 듯. **확인:** `state_machine.rs`에서 PROBLEM bit이 retry/stop을 트리거하지 않아야 함.

### 4.3 DLY + STOP race ⚠️ 검증 필요

- upstream: `38186d00` (2017-03) "DELAY wins" — STOP을 driver에 보내되 DELAY 만료까지 대기.
- motor-rs: `DelayWait` phase, `DELAY_REQ/DELAY_ACK` flag 있음. STOP이 DelayWait phase에서 DELAY를 취소하지 않고 driver stop만 보내는지 확인 필요. issue #5 해결책과 동일해야 함.

### 4.4 jog 중 컨트롤러 자체 정지 ⚠️ 검증 필요

- upstream: `9c8a8e8c` (2018-06) 컨트롤러가 internal limit으로 stop → record가 JOGF/JOGR/HOMF/HOMR clear.
- motor-rs: `command_planner.rs`/`state_machine.rs`에서 `MipFlags::JOGF | JOGR` intersect 처리는 있으나 driver-원인 정지 시 clear 경로가 명시 구현되어 있는지 검증 필요.

### 4.5 LVIO 상태에서 jog 재거부 ⚠️ 검증 필요

- upstream: `9e5b5432` (2018-08, PR #99) jog 후 LVIO로 멈춘 후 같은 버튼 재시도 거부 (limit 안쪽 방향은 허용).
- motor-rs: 명시 코드 보이지 않음. 검증 필요.

### 4.6 DLLM > DHLM → LVIO=1 ❌

- upstream: `270347df` (2018-08, PR #108).
- motor-rs: `command_planner.rs:158`은 `dhlm == dllm == 0.0`일 때만 검사를 skip. inverted(dllm>dhlm) 케이스에서 LVIO=1을 자동 set하는 분기 없음.
- **조치:** `check_soft_limits` 또는 limit 변경 path에 inverted 감지 추가.

### 4.7 URIP=Yes + RDBL error → STOP ❌

- upstream: `db5da2f0` (2017-05) RDBL link 에러 시 motor stop; `7493d50b` (2018-04) 새 move 시작 안 함.
- motor-rs: `rg 'urip.*error|rdbl.*error|rdbl.*stop' src/` → 매치 없음. URIP=Yes에서 RDBL 입력 실패 처리 분기 없음. 안전 위반 가능 (frozen RDBL로 retry가 큰 이동 유발).
- **조치:** RDBL 입력 source 도입 시 error 처리 분기 필요 (`record/status_update.rs`).

### 4.8 VAL/HOMF/VAL 무한루프 방지 ⚠️ 검증 필요

- upstream: `0aaf02d7` (2025-02, PR #224). 이동 중 HOMF set 후 VAL 재기록 시 `HOMF/HOMR` reset.
- motor-rs: `command_planner.rs`에서 VAL write가 HOMF/HOMR을 clear하는지 검증 필요.

### 4.9 음수 BDST + RTRY!=0 + 음방향 relative move ⚠️ 검증 필요

- upstream: `524696a8` (2021-11, PR #182, issue #181).
- motor-rs: `parity_backlash.rs`가 부호 케이스를 cover하는지 검토. 음수 BDST + RTRY>0 + relative 시나리오 추가 권장.

### 4.10 home + soft limit error check 제거 ⚠️ 검증 필요

- upstream: `dbcf4bc2` (2011-10), `85511023` (2013-06). home 시 soft limit error check 비활성, HVEL/BVEL/ACCL로 home accel 계산.
- motor-rs: `rg 'home.*lvio|home.*limit' src/` → 매치 없음. home 시 limit check가 건너뛰어지는지 확인 필요.

### 4.11 encoder ratio MRES/ERES sign 보존 ✅

- upstream: `928f79fc` (2017, PR #84, issue #82) `fabs()` 제거.
- motor-rs: `coordinate.rs`에 MRES/ERES sign 처리가 있어 보임 (parity_set_mode.rs로 검증). 추가 검증 권장.

### 4.12 MRES 변경 시 user/dial limit 재계산 ❌

- upstream: `2e89b552` (2022, PR #193) RHLM/RLLM 도입과 함께. issue #191 핵심.
- motor-rs: MRES put 시 limit 재계산 분기가 보이지 않음 (`rg 'mres.*change|set_mres'` 매치 없음).
- **조치:** RHLM/RLLM 도입 PR과 묶어서 처리.

### 4.13 ACCL when VELO==VBAS ⚠️ 검증 필요

- upstream: `b201e40e` (2015-08, PR #75). VELO==VBAS이면 `ACCL = VELO/TIME` (0 divide 회피).
- motor-rs: velocity 계산 path 확인 필요.

### 4.14 FLNK = DMOV False→True transition만 ⚠️ 검증 필요

- upstream: `0ef39053` (2015-02), `c970afbf` (2016-09).
- motor-rs: `suppress_flnk` 메커니즘 있음 (`record/mod.rs:34`, `parity_ntm.rs:96`). DMOV 토글 시점에 fire하는지, 같은 위치 move (`60695757` 2015) 처리도 확인.

### 4.15 UEIP auto-reset (encoder absent) ✅ 있음

- upstream: `24a53e66` (2021-11), `a4a6dbdd` (2024-10) post(ueip) 오타 fix.
- motor-rs: `field_access.rs:829-830` "if no encoder present, override UEIP back to No" 주석 있음. 구현 확인.

### 4.16 RDBD validation order, MRES==RDBD edge case ⚠️ 검증 필요

- upstream: `cf984d50` (2007-04), `17168a52` (2006-06).
- motor-rs: `parity_retry.rs`의 cover 범위 검증 필요.

---

## 5. Unmerged Closed PR — motor-rs 정책 결정 항목 (P3)

| PR# | 제목 | motor-rs 결정 필요 |
|---|---|---|
| #19, #26, #31 | homing 중 LS=done 처리 | LS bit이 home 완료 의미를 가지면 false done 위험. 명시 정책 필요 |
| #80, #81 | MSTA bit 15로 driver→record VBAS=0 요청 | issue #76(OPEN)과 연관 |
| #127 | retry RDBD 비교를 `>=` → `>` | 현재 `parity_retry.rs`가 어느 비교 쓰는지 확인 |
| #165 | init 시 stale RBV (issue #164) | device support init 동기성 |
| #171 | JOGF/JOGR 동시 활성 | issue #170(OPEN) — latest-wins 권장 |
| #188 | profileTimes_ writing | profile move 시간 배열 처리 |
| #228 | softlimit read/write | body 비어있음 |
| #211 (OPEN) | SoftMotor MINP | motor-rs가 soft motor 지원 시 검토 |

---

## 6. OPEN issue — motor-rs parity 영향도 (P2/P3)

| # | 제목 | 영향도 |
|---|---|---|
| **#170** | JOGR+JOGF 동시 활성 동작 미정의 | high — 정책 결정 |
| **#196** | autosaved MRES mismatch → 잘못된 위치 복원 | high — MRES 변경 + autosave 인터록 |
| **#231** | LOAD_POS 실패 시 DVAL/OFF 불일치 → 차단 필드 | high — invariant 추가 |
| #192 | RRBV/REP/RMP 32-bit | medium — Rust는 i64 자연, PV 노출 모델 결정 |
| #76 | VBAS-not-supported MSTA bit | medium — base class API |
| #230 | Model 3 PCO API | medium — #248 보강 |
| #54 | motorDeviceDriver.html 본문 누락 | low — doc |
| #120 | devSoftMotor soft_init 단순화 | low |

---

## 7. 빈번한 버그 패턴 — motor-rs 설계 입력

upstream issue/commit에서 반복되는 5개 패턴:

1. **autosave + MRES/ERES 위치 복원** — issue #85, #151(RSTM), #191, #196, #218, #231. invariant: "LOAD_POS 실패 시 DVAL/OFF 동기화"와 "MRES 변경 시 limit/position rescale owner"를 명시 필요.
2. **STOP / abort 의미론** — issue #5(DLY+STOP), #41(FLNK semantics), #153(backlash). 결정: STOP은 backlash 안 함, DLY 타이머 취소(또는 wait), FLNK 정상 실행.
3. **RA_PROBLEM 자동 stop** — issue #25, #100→PR #109. **driver 책임**으로 명시.
4. **Limit-switch staleness + tweak-past-limit** — issue #12, #27, #35, #205(PR #206), #212. CDIR 무관 LS 갱신, TWR/TWF/VAL/DVAL 모든 경로 limit 재검사.
5. **device support init race** — issue #61, #85, #164(PR #165 unmerged), #213. 동기적 init 권장; ERES==0 guard (PR #214).

---

## 8. R7-4 (2026-03-09) 이후 변경 — 가장 최신 catch-up

| commit / PR | 변경 |
|---|---|
| `11229ed6` 2026-02-12 / PR #236 | RVEL bug — `motorVelocity_` setpoint를 `status_.velocity`에 기록 (이후 #238로 의미 보정) |
| `314ef89a` 2026-02-13 / PR #238 | **motorActVelocity_ 신설** — setpoint와 actual velocity 분리. RVEL의 source는 actual |
| `05b25c1d` 2026-03-21 / PR #248 | **Position Compare** — `enablePCO(bool)` 가상 메서드, `PCO_START_POSITION/END/INCREMENT/PULSE_WIDTH/ENABLE` 5개 파라미터. Aerotech/Newport XPS/Galil/ACSMotion 지원. `LAST_MOTOR_PARAM`/`NUM_MOTOR_DRIVER_PARAMS` 매크로 제거 (asyn 4.32 미만 미지원) |
| `6c370602` 2026-04-06 / PR #250 | RSTM 문서 갭 — `#Fields_init` 섹션 신설 (doc-only) |

motor-rs는 RVEL/actVelocity 쌍을 처음부터 분리해서 도입하면 #236 회귀를 피할 수 있다.

---

## 9. 권장 작업 로드맵

**Sprint 1 — DBD parity (P1)**
1. ACCS / ACCU + menu motorACCSused (Accl/Accs) + ACCU readback→control 의미 (autosave) — `7291b556` 동기화 규칙 포함
2. RSTM + menu (Never/Always/NearZero/Conditional) — device support init에 분기 추가
3. RHLM/RLLM as PV + MRES change → 자동 user/dial limit 재계산 (#191/#193/#206 chain)
4. SYNC field (VAL/DVAL/RVAL ← RBV/DRBV/RRBV)

**Sprint 2 — 동작 회귀 위험 (P0)**
1. **MIP_EXTERNAL** set/check 로직 (`#ea063f5f`)
2. **DLLM > DHLM → LVIO=1** (`#270347df`)
3. **URIP=Yes + RDBL error → STOP / suppress move** (`#db5da2f0`, `#7493d50b`)
4. VAL write 시 HOMF/HOMR clear (`#0aaf02d7`)
5. jog 중 컨트롤러 자체 정지 → JOGF/JOGR/HOMF/HOMR clear (`#9c8a8e8c`)
6. LVIO 상태에서 jog 재거부 (`#9e5b5432`)
7. home 시 soft limit error check 비활성 (`#dbcf4bc2`, `#85511023`)
8. ACCL when VELO==VBAS (`#b201e40e`)

**Sprint 3 — asynMotor base 신규 (P2)**
1. **motorActVelocity_** asyn parameter (RVEL source 분리)
2. **enablePCO + PCO_*** (선택 — fly-scan 필요 시)
3. **moveToHome** framework (`#a6f64591`, `#5f421e9a`)
4. **VBAS-not-supported MSTA bit** (issue #76)

**Sprint 4 — 정책 결정 (P3, OPEN issue)**
1. #170 JOGR+JOGF 동시 — latest-wins
2. #196 autosaved MRES mismatch — MRES change 시 autosaved position 무효 처리
3. #231 LOAD_POS 실패 시 DVAL/OFF invariant
4. #192 RRBV/REP/RMP — Rust 측은 i64로 처리 (DBF_INT64 호환 PV 노출 시 결정)

**Sprint 5 — README/문서 보정**
1. `MotionPhase` 9개 enum이 README와 일치하도록 (`BacklashApproach` → `DelayWait`)
2. RSTM 도입 후 `docs/motorRecord.html`의 `#Fields_init` 구조 참고 (PR #250)
3. MRES 변경 시 limit 자동 재계산 문서화 (PR #227)

---

## 10. 종합 평가

**구현 품질:** 핵심 motion semantics (state machine, retry 4-mode, backlash 2-stage, NTM/NTMF, SPMG, SET/FOFF, coordinate, MSTA/MIP wire-호환)는 견고. 8819 LOC + 139 테스트(parity_* 5개 + scenarios + integration) 로 검증 폭이 넓다. AxisRuntime의 single-task-per-axis 모델은 C 코드의 lock 모델을 개선한 좋은 선택.

**완성도:** R7-0(2018) ~ R7-2(2020) 시점 수준. **2020-06 이후 도입된 모든 신규 필드(RSTM, RHLM/RLLM, ACCS/ACCU 일부)와 2026 신규 기능(motorActVelocity, PCO) 미반영**. 또한 외부 move 인식(MIP_EXTERNAL), inverted limit 감지, URIP error 처리 같은 안전성 항목 누락.

**Parity 갭의 영향:**
- 운영 환경에서 IOC가 R7-3/R7-4 기준 autosave/MEDM/PyDM 스크린을 그대로 쓰면 ACCS/RSTM 필드 부재로 caput 에러.
- driver가 외부적으로 move 시작하면 record dmov가 따라가지 않아 ophyd 같은 client가 hang.
- URIP=Yes 환경에서 IOC가 죽으면 motor-rs는 frozen RDBL로 retry를 시도해 안전 위반 가능.

**결론:** motor record의 정수(coordinate, state machine, retry, backlash)는 잘 포팅되어 있다. 하지만 "잘 구현되었는지"의 기준이 **R7-4 (2026-03) parity** 라면, 위 §9 Sprint 1+2 (DBD 4개 필드 + 동작 8개 항목)을 완료해야 한다. Sprint 3 (PCO/moveToHome)는 fly-scan/다축 시퀀스 사용 여부에 따라 결정.

---

부록: 데이터 소스
- 머지 PR 분석: 110개 (PR #1~#250 중 머지된 것)
- Issue 분석: 106개 (open 11 / closed 95)
- motorRecord.cc commit 151개 시계열 (2000~2026)
- motorRecord.dbd commit 42개 시계열
- asynMotorController/Axis commit 75개 시계열
- motor-rs grep verification (SPDB/ACCS/RSTM/SYNC/MIP_EXTERNAL/RVEL/PCO 등)
