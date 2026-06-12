# motor-rs C-parity 잔여 항목 (round-3 sweep 종결 기록)

C `epics-modules/motor` `motorRecord.cc`/`motorRecord.dbd` 대비 round-3
parity sweep (2026-06, families #6–#15)에서 나온 항목 중, 코드 수정으로
닫히지 않고 **차단(blocked)** 또는 **의도적 deviation** 으로 종결된
것들의 영구 기록입니다. 이 문서는 코드 베이스와 대조해 verify된 결과만
기록 — 추정/희망 verdict 금지. 수정 완료된 항목(U1–U12 포함 28개
finding)은 git log의 `fix(motor):` 커밋이 원장이며 여기 중복하지
않습니다.

## ~~BLOCKED — published API 동결~~ (0.20.0 major bump으로 해제)

H2(PID gain forward)와 H3(soft-limit forward)는 `MotorCommand`
public exhaustive enum의 variant 추가가 semver-major라서 0.19.x에서
차단되어 있었다. 사용자가 major bump(0.19.2 → 0.20.0)를 승인하여
둘 다 이식 완료 — `MotorCommand::SetPidGain`/`SetHighLimit`/
`SetLowLimit` variant와 `AsynMotor` 기본 구현 메서드로 닫힘. 상세는
git log의 `feat(motor):` 커밋이 원장.

## ~~BLOCKED — 프레임워크 구조 (epics-base-rs, cross-crate)~~ (epics-base-rs 구조 변경으로 해제)

H6 잔여 두 건 모두 epics-base-rs 구조 변경으로 이식 완료:

- `MonitorEvent`가 per-event DBE mask를 운반하고 (coalesce 시 OR 누적,
  `DbSubscription::recv_event`로 구독측 수신 — BRIDGE-79 선행 차단
  해제), `ProcessSnapshot.changed_fields`가 필드별 posting mask를
  실어 C `db_post_events(field, mask)`의 per-field 협폭이 복원됨
  (RBV: MDEL 교차 → `DBE_VALUE`, ADEL 교차 → `DBE_LOG` 독립; MARK
  상당 필드: `DBE_VAL_LOG`).
- alarm-only cycle posting은 `Record::alarm_cycle_monitored_fields`
  (motor가 C `monitor()` 3513-3645의 posting list를 반환)로 닫힘 —
  alarm 전이가 있는 패스에서 미변경 구독 필드도 `DBE_ALARM`으로 post.

상세는 git log의 `feat(base):`/`feat(motor):` 커밋이 원장. BRIDGE-79
본체(qsrv group monitor의 leaf 협폭 — `MemberEvent` 분류 +
pvdata/encode leaf-family marking)는 bridge-rs 측 잔여 작업으로 이
문서 범위 밖.

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
| special()-transport pass (PCOF/ICOF/DCOF, JVEL, PCO_ENABLE) | special()이 put 시점에 driver로 직접 전송, process pass 없음 (pidcof 3003-3026, JVEL 3059-3072; PCO는 C dbd에 없음) | motor-rs는 put-time driver channel이 없어 put 직후의 process pass가 명령 transport — 이 5개 필드만 `process_passive.rs`의 motor pp set에 extension으로 유지되어 put마다 pass 1회가 돈다 (C에 없는 FLNK/monitor/암묵 GET_INFO 동반). 나머지 non-pp 필드는 gate가 pass 자체를 차단해 C와 동일 |

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
  C의 return(OK)처럼 발화하지 않음). C dbd에 pp가 없는 필드(SET,
  SSET/SUSE, FOFF, VELO/BDST 등 config 전부)는 epics-base-rs
  `process_passive.rs`의 motor pp(TRUE) set 모델링으로 put 시 process
  pass 자체가 돌지 않는다 — 모델링 전에는 legacy always-process가
  모든 put에 pass(+FLNK/monitor/암묵 GET_INFO)를 돌렸다. 예외 5개
  필드(PCOF/ICOF/DCOF/JVEL/PCO_ENABLE)는 위 "의도적 deviation" 표의
  special()-transport 행 참조.
