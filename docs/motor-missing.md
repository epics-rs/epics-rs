# motor-rs C-parity 잔여 항목 (round-3 sweep 종결 기록)

C `epics-modules/motor` `motorRecord.cc`/`motorRecord.dbd` 대비 round-3
parity sweep (2026-06, families #6–#15)에서 나온 항목 중, 코드 수정으로
닫히지 않고 **차단(blocked)** 또는 **의도적 deviation** 으로 종결된
것들의 영구 기록입니다. 이 문서는 코드 베이스와 대조해 verify된 결과만
기록 — 추정/희망 verdict 금지. 수정 완료된 항목(U1–U12 포함 28개
finding)은 git log의 `fix(motor):` 커밋이 원장이며 여기 중복하지
않습니다.

## BLOCKED — published API 동결 (motor-rs 0.19.2)

`MotorCommand`는 public **exhaustive** enum (`flags.rs:248`,
`#[non_exhaustive]` 없음)이고 `MotorDriver` trait와 함께 0.19.2로
crates.io에 publish됨. variant/메서드 추가는 downstream exhaustive
match를 깨는 semver-major. major bump 전까지 다음 driver forward는
emit 불가:

| 항목 | C 출처 | Rust 현재 동작 |
|---|---|---|
| H2 — PID gain forward (`SET_PGAIN`/`SET_IGAIN`/`SET_DGAIN`) | `motorRecord.cc` special pidcof (3003-3026): GAIN_SUPPORT일 때 0.0–1.0 clamp 후 driver로 전송 | `field_access.rs` PCOF/ICOF/DCOF arm — clamp + raw store만 수행, command 미전송 (소스 주석에 명시) |
| H3 — soft-limit forward (`SET_HIGH_LIMIT`/`SET_LOW_LIMIT`) | `set_dial_highlimit`/`set_user_highlimit` (4101-4160대): DHLM/DLLM/HLM/LLM 변경을 device로 전송 | limit put은 record-내부 cascade만 수행, driver command 미전송 |

Major bump 시점에 `MotorCommand::SetPidGain { which, gain }` /
`SetSoftLimit { high, low }` 류 variant + `MotorDriver` 기본 구현
메서드로 닫는다.

## BLOCKED — 프레임워크 구조 (epics-base-rs, cross-crate)

| 항목 | C 동작 | 구조적 차단 지점 |
|---|---|---|
| H6 잔여 — per-field DBE mask 협폭 posting | `monitor()`이 MARK된 필드별로 `db_post_events(field, mask)` — 구독자는 필드×마스크 단위로 수신 | `epics-base-rs` `MonitorEvent`가 `{snapshot, origin}`만 운반, per-event mask 미전달 (BRIDGE-79와 동일 차단). record-wide posting mask는 유지되어 구독자는 mask 교집합 필터만 가능 |
| H6 잔여 — alarm-only cycle posting | alarm 변화만 있는 process cycle에서 deadband 필드/MARK 필드를 `DBE_ALARM` mask로 post | 전 record type 공통의 기존 프레임워크 deviation — snapshot 단위 게시 구조에서는 alarm-only 판별이 post 지점에 없음 |

둘 다 `MonitorEvent`에 mask를 싣는 epics-base-rs 구조 변경(BRIDGE-79
선행 작업)이 닫는다. motor-rs 단독으로는 수정 불가.

## OUT OF SCOPE — motor sweep 외부

| 항목 | 내용 |
|---|---|
| H4-인접 — ai/ao/longin per-field metadata | motor는 H4(5335be38)로 필드별 graphic/control metadata를 dbd 기준으로 serve하지만, 다른 record type(ai/ao/longin 등)은 여전히 per-record 근사 metadata를 serve. C는 그 타입들도 per-field로 keying (각 upstream dbd `prop(YES)`). motor sweep 범위 밖 — epics-base-rs/해당 record 크레이트의 별도 finding |

## 의도적 deviation (검증 완료, 수정하지 않음)

| 항목 | C 동작 | Rust 동작 + 근거 |
|---|---|---|
| G4 — boot-while-moving DMOV | `init_record` (733-737): 초기 readback **후** `dmov = TRUE; movn = FALSE` 강제 — 축이 물리적으로 이동 중이어도 1 poll 동안 idle로 표시, 다음 poll에서 external-move 감지 (ea063f5f) | `initial_readback` (`status_update.rs:420`): `dmov = done && !moving`; `process_motor_info`의 EXTERNAL 블록(186-191)이 초기 readback 패스에서 즉시 `MIP_EXTERNAL + dmov=0`. 정상 상태 수렴은 C와 동일 (EXTERNAL 완료 시 readback reseed + dmov=1), 차이는 boot 1-poll 윈도의 DMOV/MOVN 값뿐 — C는 boot artifact(같은 블록의 "MSTA incorrect at boot-up" 주석), Rust는 실제 상태를 보고 |
| F5 잔여 — coalesced latent-SPMG-Go | C는 put마다 process 1회 — SPMG 전이가 다른 put과 같은 패스에 겹치지 않음 | 프레임워크 put-coalescing으로 **3개 이상**의 미처리 put이 한 패스에 합쳐질 때만 SPMG 전이가 parked되어 latent gate가 다음 패스에 replay. micro-deviation으로 문서화 (전이 자체는 소실되지 않음) |
| U2 — RRBV/RMP/REP 선언 타입 | dbd `DBF_LONG` (32-bit) | `Int64` 선언 — 64-bit raw count 표현을 위한 의도적 Rust extension (round-3에서 intentional로 종결) |
| in-flight retarget 즉시 emit | do_work 이동 dispatch가 `mip == DONE \|\| RETRY`로 gate — 새 target은 park 후 완료 시 dispatch (2455) | `RetargetAction::ExtendMove`: on-the-fly retarget 컨트롤러를 위해 즉시 emit + 완료 시 verify (`command_planner.rs` ExtendMove arm에 deliberate divergence로 명시, 이전 sweep에서 종결) |

## 관련 결정 기록 (이번 closeout에서 dbd-faithful로 정렬)

- VELO/BVEL/ACCS 기본값: dbd에 `initial()` 없음 → 0.0 (73481abd).
  JVEL/HVEL과 동일 컨벤션, UREV=0.0 sentinel 결정과 동일 방향.
  configured record는 init pass가 S/SBAK/ACCL에서 도출하므로 무영향.
- driverless STUP: C 1824-1828 (GET_INFO 불가 device → OFF 복귀)에
  맞춰 device_state 부재 시 BUSY 진입 차단 (ab8ee3b3). C의
  NOTHING_DONE 암묵 GET_INFO 분기(2546-2557)는 이후 이식 완료 —
  no-op put/scan pass(`None` arm, `put_pass=true`)가 chain end에서
  STUP→BUSY + status refresh를 발화하고, CALLBACK_DATA pass는
  process_reason 판별자(`internal.idle_status_pass`)로 제외되어 C와
  동일한 poll-feedback 방지를 가진다. housekeeping put pass도 이식
  완료: plan_motion의 CNEN/SPMG arm과 jog/home/closed-loop의
  미소비(unconsumed) leg가 C do_work의 in-block return 구조 그대로
  chain end로 떨어져 같은 pass에서 암묵 GET_INFO를 발화한다 (소비된
  pass — 이동 dispatch, soft-limit 거부, Stop/Pause top block — 는
  C의 return(OK)처럼 발화하지 않음). 남은 생략은 C dbd에 pp가 없어
  process pass 자체가 존재하지 않는 필드(PCOF/ICOF/DCOF, SET,
  SSET/SUSE, FOFF)와 C dbd에 없는 Rust 확장 PCO 필드뿐 — C도 해당
  put에서 pass를 돌지 않으므로 deviation이 아니다.
