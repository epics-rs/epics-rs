# asyn-rs Parity Review — epics-modules/asyn C↔Rust 비교

분석일: 2026-06-12
대상: `asyn-rs` (~/codes/epics-rs/crates/asyn-rs) — core device-support/record path
upstream: `epics-modules/asyn` HEAD `e2a281e2` (2025-08-04)

대상 범위: manager/port/interrupt, param library, devEpics device support, asyn record, interfaces/interpose
범위 밖: hardware drivers (ftdi / ip_port / ip_server_port / prologix / serial_port / usbtmc / vxi11)

데이터 수집: multi-agent C↔Rust comparison (asynManager.c, asynPortDriver.cpp/paramVal.cpp, devAsynInt32/Float64/Int64/Octet/UInt32Digital/XXXArray, asynRecord.c, asynEpicsUtils.c, asynInterpose*.c ↔ 대응 Rust 모듈).

---

## STATUS — VERIFIED 2026-06-29: ALL 17 FINDINGS CLOSED (doc SUPERSEDED)

This 2026-06-12 inventory predates ~25 caucus review rounds of asyn-rs fix
work. A 3-panel opus verification round (manager/port+interpose, param+convert
device-support, devsup+record) re-checked every confirmed gap against current
`main` and the upstream C. **Every one is CLOSED** — each by a post-review fix
commit (most dated within two days of this review), with current-code file:line
evidence and C-reference confirmation. No STILL-OPEN finding remains on the
reviewed surface; this doc is retained only as a historical record. For NEW
work, re-audit from the C surface rather than trusting the gap list below.

| § | Finding id | Fixing commit(s) |
|---|---|---|
| 1 | deadline-reorders-within-priority | `cbd8a372` |
| 1 | lifecycle-ops-gated-by-block-holder | `ac0787f5` |
| 1 | no-2s-autoconnect-backoff | `115b0153` |
| 1 | deadline-aborts-dequeued-request | `3e9a6514` |
| 1 | enable-does-not-refuse-defunct-port | `c6045740` |
| 6 | delay-trailing-and-single-char-delay-omitted | `abbfde36` |
| 3 | read-discards-stored-param-status | `9d8f1512` |
| 3 | flush-missing-isdefined-gate | `fe4d7fa0` |
| 4 | io-intr-drops-driver-alarm-status-severity | `0b9e8d54` / `a50d3f73` |
| 4 | asynfloat64-ai-skips-smoo-aslo-aoff | `14b535ad` |
| 4 | asynint32-ai-linear-eslo-eoff-never-applied | `614e7ebb` / `c850c287` |
| 4 | init-seed convert routing (ao/ai readback) | `6812c0c0` / `d4e8f66e` |
| 4 | driver-enum-string-tables-not-propagated | `1b63540e` |
| 4 | asynmask-nbits-bipolar-not-supported-for-int32 | `5913972c` |
| 4 | parselink-addr-base0-hex-not-accepted | `28e75f65` |
| 5 | octet-read-error-leaves-status-fields-stale | `09852c48` |
| 5 | dbit-readback-wrong-option-key-csize | `678e272c` |
| 5 | read-error-no-read-alarm-or-overflow-minor | `170c327b` |

Residual STATE_ALARM on AQR-cancel / queue-timeout = the separately-classified
§7 non-gap (`queue-timeout-and-not-connected-no-error-alarm`), not a new
finding. Out-of-scope surface untouched by this review and not yet audited:
hardware drivers (ftdi / ip_port / ip_server_port / prologix / serial_port /
usbtmc / vxi11).

---

## 0. TL;DR

asyn-rs의 핵심 비동기 재설계(actor-per-port, coalescing mailbox, RequestOp 디스패치)는 구조적으로 견고하며, manager/port 큐, param library, octet I/O, interpose 스택의 골격은 C와 매핑된다. 그러나 **C의 software-side 변환(device support)·상태 전파(param status→alarm)·정책(throttle/FIFO)·readback 키** 레벨에서 17개의 확정 동작 갭이 남아 있다.

확정 갭(real_gap=true): **17개**

| severity | count | ids |
|---|---|---|
| medium | 13 | lifecycle-ops-gated-by-block-holder, no-2s-autoconnect-backoff, read-discards-stored-param-status, flush-missing-isdefined-gate, io-intr-drops-driver-alarm-status-severity, asynfloat64-ai-skips-smoo-aslo-aoff, asynint32-ai-linear-eslo-eoff-never-applied, driver-enum-string-tables-not-propagated, octet-read-error-leaves-status-fields-stale, dbit-readback-wrong-option-key-csize, read-error-no-read-alarm-or-overflow-minor, delay-trailing-and-single-char-delay-omitted, deadline-reorders-within-priority |
| low | 4 | deadline-aborts-dequeued-request-vs-c-timeoutuser, enable-does-not-refuse-defunct-port, asynmask-nbits-bipolar-not-supported-for-int32, parselink-addr-base0-hex-not-accepted |

가장 사용자-영향이 큰 군집:
- **Device support software 변환 누락** — asynFloat64 ai SMOO/ASLO/AOFF, asynInt32 ai LINEAR ESLO/EOFF, asynInt32 @asynMask nbits/bipolar가 read 경로에서 적용되지 않아 VAL이 raw 값/미스케일로 표시.
- **Alarm/status 전파 단절** — param 저장 status, I/O Intr 콜백 alarm, asyn record I/O 에러가 record SEVR로 올라가지 않음.
- **asyn record octet 에러 경로** — 실패/타임아웃 시 NORD/EOMR/AINP/BINP/NAWT가 직전 성공 값으로 잔류.

거부된(non-gap) 9개 findings는 §7에 검증 결과와 함께 보존.

---

## 1. Manager / Port

| id | sev | C ref | Rust ref |
|---|---|---|---|
| deadline-reorders-within-priority | medium | asynManager.c:869-898, 1612-1613 | port_actor.rs:78-85 |
| lifecycle-ops-gated-by-block-holder | medium | asynManager.c:2222-2249, 2310-2324, 1322-1355, 2326+ | port_actor.rs:180-190, 226-241 |
| no-2s-autoconnect-backoff | medium | asynManager.c:704-739 | port_actor.rs:258-271 |
| deadline-aborts-dequeued-request-vs-c-timeoutuser | low | asynManager.c:827, 906, 647-687 | port_actor.rs:247-253 |
| enable-does-not-refuse-defunct-port | low | asynManager.c:2236-2243 | port.rs:708-718 |

### deadline-reorders-within-priority — medium
- C portThread는 각 priority 큐를 strict FIFO로 서비스한다(queueRequest가 `queueList[priority]` 꼬리에 ellAdd, ellFirst→ellNext로 첫 eligible 노드 선택). Rust `Ord::cmp`는 priority 다음에 `other.deadline.cmp(&self.deadline)`(nearer-deadline-first) 타이브레이커를 끼워, timeout이 다른 동-priority 요청이 제출 순서를 거슬러 reorder된다(짧은 timeout의 나중 요청이 먼저 pop).
- **Fix:** Ord에서 deadline 타이브레이커 제거 → ordering을 priority-then-seq(FIFO)로 축약.

### lifecycle-ops-gated-by-block-holder — medium
- C에서 blockProcessCallback은 portThread의 I/O processUser 디스패치(`pblockProcessHolder` gate, 887-895)만 막는다. enable/autoConnect/isConnected/connectDevice/disconnect는 큐를 거치지 않고 asynManagerLock 하에서 즉시 dpCommon을 변경한다. Rust는 이들을 일반 actor 메시지로 두고, `enqueue_message`가 block_token != owner인 모든 메시지를 `pending_while_blocked`로 우회시켜, 비-소유자 caller의 set_enable/set_auto_connect/connect가 UnblockProcess까지 stall된다.
- **Fix:** state/lifecycle 메시지(SetEnable/SetAutoConnect/GetEnable/GetAutoConnect/Connect/Disconnect/…)를 block divert에서 제외 — C처럼 큐 밖에서 즉시 처리.

### no-2s-autoconnect-backoff — medium
- C autoConnectDevice는 `lastConnectDisconnect` 이후 2.0초 미만이면 reconnect를 거부하고, `autoConnectActive`로 재진입을 막아 offline 디바이스의 재접속 폭주를 2초당 1회로 throttle한다. Rust actor는 disconnected auto_connect 포트로 디스패치되는 모든 non-connect 요청마다 `driver.connect()`를 무조건 호출(elapsed gate·in-progress flag 없음) → 큐된 N개 요청이 N회 연속 full connect 시도.
- **Fix:** port base에 `last_connect_disconnect`(WallTime)와 in-progress 플래그를 추가, 2초 backoff + 재진입 가드를 dispatch 전에 적용.

### deadline-aborts-dequeued-request-vs-c-timeoutuser — low
- C 표준 device support는 `queueRequest(...,0)`으로 큐 타임아웃을 절대 arm하지 않아, head에 도달한 요청은 항상 I/O를 실행한다. Rust는 heap pop 후 deadline을 재검사해(`process_one` 247), 느린 선행 op가 actor를 점유해 deadline이 지난 경우 I/O 실행 전 asynTimeout으로 거부.
- **Fix:** I/O timeout을 큐 pre-execution deadline으로 재사용하지 말 것 — pop 후 deadline 거부 분기를 제거하고 timeout은 driver I/O에만 적용(backpressure는 별도 정책).

### enable-does-not-refuse-defunct-port — low
- C enable()은 `pdpCommon->defunct`를 먼저 확인해 shutdown된 포트에서 asynDisabled를 반환하며 enabled를 변경하지 않고 exception도 안 쏜다. Rust `PortDriver::enable/disable` 기본 구현은 defunct 가드 없이 base.enabled를 토글하고 asynExceptionEnable을 fan-out(SetEnable이 is_connect_op이라 check_ready의 defunct gate도 우회). I/O는 check_ready의 defunct-first 테스트로 여전히 막히므로 divergence는 반환 status + spurious exception에 한정.
- **Fix:** enable/disable 진입부에 defunct 가드 추가 → defunct면 asynDisabled 반환, 토글·exception 없이 early return.

---

## 2. Interrupt

확정 갭 없음. 이 서브시스템에서 제기된 3개 findings(delivery-order, per-param timestamp, interruptAccept gate)는 모두 검증 후 비-갭으로 분류 — §7 참조.

---

## 3. Param library

| id | sev | C ref | Rust ref |
|---|---|---|---|
| read-discards-stored-param-status | medium | asynPortDriver.cpp:309,320,336,364,391,550,578 | port_actor.rs:700-715, port.rs:793-832, request.rs:446-456 |
| flush-missing-isdefined-gate | medium | asynPortDriver.cpp:845 | port.rs:596-666, param.rs:1015-1049, 1080-1083 |

### read-discards-stored-param-status — medium
- C paramList::getInteger/getDouble/…는 param의 저장된 asynStatus(`pVal->getStatus()`)를 read의 반환 status로 돌려주고, devAsyn* device support는 non-success read status를 READ/INVALID alarm으로 매핑한다. Rust `get_*_strict`는 param이 defined면 stored status를 보지 않고 무조건 Ok(value)를 반환, read post-processing이 `get_param_status`를 `(_, alarm_status, alarm_severity)`로 destructure하며 AsynStatus를 버린다 → setParamStatus(error/timeout)-only 한 defined param이 clean read(status=Success, alarm=(0,0))로 읽혀 UDF가 clear됨.
- **Fix:** read 경로에서 entry.status를 RequestResult.status로 전파하고, defined param의 non-success status도 asyn_error_to_alarm 매핑이 적용되도록 read post-processing을 수정.

### flush-missing-isdefined-gate — medium
- C paramList::callCallbacks는 dirty 플래그를 순회하되 `if (!param->isDefined()) continue;`로 changed-but-undefined param을 건너뛴다. Rust flush(`call_param_callbacks`/`call_param_callback`)는 defined 체크 없이 모든 changed reason에 대해 InterruptValue를 emit → 값이 한 번도 set된 적 없는 scalar에 setParamStatus/Alarm(또는 bare mark_changed)이 spurious I/O Intr를 발생(Array는 set_*_array가 항상 defined=true라 영향 없음).
- **Fix:** 두 flush 메서드의 emit를 `params.is_param_defined(reason, addr)`로 게이트 — C의 continue와 동일.

---

## 4. Device support (devEpics)

| id | sev | C ref | Rust ref |
|---|---|---|---|
| io-intr-drops-driver-alarm-status-severity | medium | devAsynInt32.c:561-563, 781, 843-847 | adapter.rs:206-210, 1049-1052, 839-862 |
| asynfloat64-ai-skips-smoo-aslo-aoff | medium | devAsynFloat64.c:594-604 | adapter.rs:616, 864-885; ai.rs:277, 604-606, 308-313 |
| asynint32-ai-linear-eslo-eoff-never-applied | medium | devAsynInt32.c:848-851, 822-828 | adapter.rs:539-576, 614, 864-885; record_trait.rs:414-429; ai.rs:277 |
| driver-enum-string-tables-not-propagated | medium | devAsynInt32.c:297-324, 415-435, 1139/1243; devAsynUInt32Digital.c:547-601 | adapter.rs:622, 719-814; port_actor.rs (EnumRead drops entries) |
| asynmask-nbits-bipolar-not-supported-for-int32 | low | devAsynInt32.c:232-247, 485-488/537-540 | adapter.rs:120-132, 1193-1197, 581/614 |
| parselink-addr-base0-hex-not-accepted | low | asynEpicsUtils.c:114, 186/193 | adapter.rs:58-64, 116-118 |

### io-intr-drops-driver-alarm-status-severity — medium
- C에서 모든 I/O Intr ring-buffer 엔트리는 alarmStatus+alarmSeverity를 실어 나르고 processXxx가 asynStatusToEpicsAlarm/recGblSetSevr로 적용한다. Rust `InterruptValue`는 alarm_status/alarm_severity를 가지지만, bridging task가 value+timestamp만 CachedInterrupt로 복사하고 IoIntr read() 분기는 last_alarm_status/severity를 건드리지 않는다 → I/O Intr record가 driver alarm을 절대 반영 못 함(polled 경로는 정상). 
- **Fix:** status(또는 사전 변환된 alarm)를 CachedInterrupt까지 운반하고 IoIntr read 분기에서 last_alarm_* 설정.

### asynfloat64-ai-skips-smoo-aslo-aoff — medium
- C devAsynFloat64 processAi는 engineering 값을 직접 계산한다: ASLO/AOFF 적용 후 SMOO 필터(`pr->val = pr->val*smoo + val64*(1-smoo)`)를 걸고 return 2(record convert skip). Rust는 raw driver double을 VAL에 직접 쓰고 `computed()`(skip_convert=true)를 반환해 ai record의 동일 SMOO/ASLO/AOFF 블록을 우회 → SMOO!=0 또는 ASLO!=1/AOFF!=0일 때 미필터/미스케일 raw 값 표시.
- **Fix:** computed() 경로에서 float64-specific ASLO/AOFF/SMOO를 software로 재현(queued·I/O-Intr 양쪽).

### asynint32-ai-linear-eslo-eoff-never-applied — medium
- C asynInt32 processAi는 raw int를 `pr->rval`에 넣고 return 0 → record convert()가 ESLO/EOFF linearization을 실행. Rust read()는 raw int를 `set_val(Long)`로 VAL에 직접 coerce해 쓰고 computed()를 반환 → init의 `apply_linear_eslo_eoff`가 계산한 ESLO/EOFF가 dead, LINR=LINEAR ai가 engineering units 대신 raw counts 표시(asynUInt32Digital→mbbi는 set_val이 RVAL+SHFT+state-table를 재적용하므로 영향 없음).
- **Fix:** asynInt32 ai read는 RVAL에 raw를 쓰고 record convert를 돌리거나, computed() 경로에서 ESLO/EOFF/SMOO를 software로 재현.

### driver-enum-string-tables-not-propagated — medium
- C int32/uint32 device support는 record가 enum 상태를 노출(mbbi/mbbo/bi/bo)하고 driver가 asynEnum을 구현하면 init에서 driver의 enum strings/values/severities를 ZRST/ZRVL/ZRSV로 setEnums하고 runtime callback도 등록한다. Rust는 asynEnum 인터페이스와 EnumRead op는 있으나 actor가 EnumEntry 테이블을 버리고(`let (idx, _entries)`), init()이 enum 엔트리를 읽지 않으며 enum-string callback도 없음 → driver-defined ZRST/ZRVL/ZRSV 미적용 + runtime 갱신 손실.
- **Fix:** RequestResult에 enum 엔트리를 운반시켜 init에서 ZRST/ZRVL/ZRSV에 put_field, 그리고 enum-string 변경 callback 등록.

### asynmask-nbits-bipolar-not-supported-for-int32 — low
- asynInt32 record에서 C는 `@asynMask`의 3번째 인자를 bit COUNT(nbits)로 해석한다: 음수=bipolar(sign extension, deviceLow/High = -2^(n-1)..2^(n-1)-1), 양수=unipolar(mask=~(~0<<nbits), 0..2^n-1). Rust는 이를 literal u32 bitmask로 파싱 → 음수 nbits는 `parse::<u32>()` 실패로 link 바인딩 자체가 안 되고, 양수는 raw mask로 처리되어 mask/sign-extend 없이 잘못된 값과 getBounds-기반 bounds.
- **Fix:** asynInt32 link 파서에서 mask 인자를 nbits로 재해석(음수 bipolar, sign-extend on read/interrupt, nbits-derived deviceLow/High가 getBounds보다 우선).

### parselink-addr-base0-hex-not-accepted — low
- C는 asyn link addr을 `strtol(...,0)`(base auto: 0x hex, 0 octal)로, mask를 `strtoul(...,0)`로 파싱한다. Rust addr 파서는 `parse::<i32>()`(decimal-only) → `@asyn(port,0x10)PARAM`은 바인딩 실패, `@asyn(port,010)PARAM`은 octal 8이 아닌 decimal 10으로 silently 잘못 바인딩(mask는 0x/0X는 처리하나 octal `010`→8은 미처리).
- **Fix:** addr/mask 파싱을 같은 crate의 strtol(base 0)-faithful 파서(`trace.rs:179-191`)로 통일.

---

## 5. asyn record

| id | sev | C ref | Rust ref |
|---|---|---|---|
| octet-read-error-leaves-status-fields-stale | medium | asynRecord.c:1557-1635, 1547 | asyn_record/mod.rs:593-637, 580-589 |
| dbit-readback-wrong-option-key-csize | medium | asynRecord.c:1883-1888 | asyn_record/mod.rs:1561 |
| read-error-no-read-alarm-or-overflow-minor | medium | asynRecord.c:1599, 1602-1621, 1334/1380/1416/1452 | asyn_record/mod.rs:580-637, 2698-2734 |

### octet-read-error-leaves-status-fields-stale — medium
- C performOctetIO는 read 분기에서 status != asynSuccess(timeout/overflow/disconnect)에도 상태/데이터 필드를 무조건 갱신한다: 입력 버퍼 memset(0), 도착 바이트를 AINP/BINP에 저장, EOMR=eomReason, NORD=nbytesTransfered, TINP escaped post; write 분기도 NAWT=nbytesTransfered를 항상 설정. Rust `record_read_result`는 Ok(result) 분기에서만 필드를 갱신하고 Err(e)는 errs만 설정 → timeout/실패 octet read 후 NORD/EOMR/AINP/BINP/NAWT가 직전 성공 transfer 값으로 잔류.
- **Fix:** error 분기에서도 zeroed/partial transfer를 반영(NORD=0/partial, AINP/BINP zeroed/partial, EOMR/NAWT 갱신) — driver가 partial count를 운반하도록 OctetReadResult를 Err에도 부착.

### dbit-readback-wrong-option-key-csize — medium
- C getOptions는 serial data-bits readback을 key `"bits"`로 읽는다(DBIT setOption write도 `"bits"`). Rust write 경로는 이미 `"bits"`로 수정됐으나 `read_options_from_driver`의 readback은 여전히 `get_option_blocking("csize")`를 호출하는데, serial driver의 get_option은 `"csize"`를 인식 못 해 항상 Err → `if let Ok` 본문 미실행, DBIT가 0(Unknown)로 잔류.
- **Fix:** mod.rs:1561 `"csize"` → `"bits"` (one-line).

### read-error-no-read-alarm-or-overflow-minor — medium
- C performIO/performOctetIO는 recGblSetSevr로 I/O 결과를 record alarm severity로 올린다: read 실패→READ_ALARM/MAJOR, ASCII/Hybrid input overflow→READ_ALARM/MINOR(+AINP NUL-truncate), missing interface→COMM/MAJOR, register write/read 에러→WRITE/READ/MAJOR. Rust process()/perform_io는 STAT/SEVR를 절대 설정하지 않고 모든 실패를 ERRS 문자열로만 보고(코드 주석이 'STATE_ALARM not modeled', 'reports I/O errors via ERRS only' 인정) → CA/PVA client의 SEVR 모니터는 C가 MAJOR/MINOR를 올리는 곳에서 NO_ALARM을 봄, overflow AINP truncation도 부재.
- **Fix:** record_*_result/apply_io_outcome에서 epics-base-rs `rec_gbl_set_sevr`로 READ/WRITE/COMM 및 overflow MINOR를 raise하고 AINP overflow NUL-truncation 구현.

---

## 6. Interfaces / Interpose

| id | sev | C ref | Rust ref |
|---|---|---|---|
| delay-trailing-and-single-char-delay-omitted | medium | asynInterposeDelay.c:41-52 | interpose/delay.rs:48-59 |

### delay-trailing-and-single-char-delay-omitted — medium
- C `asynInterposeDelay::writeIt`는 마지막 char를 포함해 매 char 뒤에 sleep한다(loop: write 1 char → sleep → transfered++). N-char write는 N sleeps, single-char write는 1 sleep. Rust `DelayInterpose::write`는 (1) `data.len() <= 1`에서 delay 없이 early-return → single-char write가 zero delay, (2) `if i > 0`로 char 2..N 앞에서만 sleep → N-1 sleeps, 마지막 char 뒤 trailing delay 없음. delay-sensitive 디바이스에서 다음 transaction을 한 `delay` 더 일찍 시작.
- **Fix:** sleep를 각 char write 뒤로 이동하고 `len() <= 1` short-circuit 제거 — single-byte도 1×delay, multi-byte는 trailing delay 포함.

---

## 7. 검증 후 비-갭 / 의도적 차이 (verified intentional / not-a-gap)

아래 9개는 분석 중 제기됐으나 검증 후 record/client-observable parity 갭이 아님으로 분류.

### Manager / Port
- **connect-priority-runs-on-disabled-port** — Not a gap: C는 connect/disconnect/enable/block lifecycle을 queueRequest를 거치지 않는 DIRECT synchronous asynManager/asynCommon 메서드로 실행해 이미 disabled 포트에서도 돈다. Rust의 is_connect_op 우회는 이를 diverge하는 게 아니라 일치시킴.

### Interrupt
- **interrupt-delivery-order-index-vs-change** — Mechanism difference(C는 flags push_back/dedup로 change-order, Rust는 enumerate로 index-order)는 실재하나 observable record/client parity 갭이 아님.
- **interrupt-timestamp-per-param-vs-port-level** — Structural API-shape 차이(per-param ts cell + port-level setTimeStamp sink 부재)일 뿐 observable 갭 아님; 기본 경로는 C와 일치하고 divergent 경로는 workspace 어디에서도 사용 안 됨.
- **interrupt-accept-init-gate-absent** — Rust에 interruptAccept gate가 없는 건 사실이나 coalescing mailbox가 그 의도된 async 구조적 등가물; 주장된 multi-process init divergence는 발생하지 않음.

### Param library
- **flush-ordering-index-vs-first-change** — Ascending-index flush order vs C first-change order는 observable record-level 갭 아님; 양쪽 모두 cross-reason invocation order를 보존하지 않는 deferred/per-subscriber 전달로 hand-off.

### asyn record
- **queue-timeout-and-not-connected-no-error-alarm** — Not a gap: STATE_ALARM severity는 기록 전반의 알려진 구조적 누락(STAT/SEVR 필드 미모델링)이고, ERRS wording 차이는 의도된 async-state-machine 재설계에서 흘러나옴.

### Interfaces / Interpose
- **average-empty-read-no-udf-invalid-alarm** — helper에 real C-contract divergence가 있으나 latent(zero callers)라 현재 record/client가 관찰하지 못함; 현시점 observable 갭 아님.
- **eos-interpose-setter-no-length-validation** — production EOS-set 경로엔 validation이 존재; 미검증 EosInterpose setter는 test-only라 record/client가 divergence를 관찰하지 못함.
- **syncio-writeread-not-exposed** — No observable gap: C writeRead의 atomic flush+write+read는 `RequestOp::OctetWriteRead`로 완전 구현되어 public PortHandle 표면에서 도달 가능; named SyncIO convenience wrapper만 부재(ergonomic, behavioral 아님).
- **echo-write-partial-count-lost-on-error** — Not observable: asyn-rs octet write_octet 계약 전체가 nbytesTransfered를 success·failure 모두 드롭하므로, 어느 record/client도 partial echo-verified count를 관찰할 수 없음.
