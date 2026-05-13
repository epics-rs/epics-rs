# asyn-rs C-source audit — 2026-05-14

22 항목 "이미 구현 (verified ✓)" 표 검증 결과. C source root: `~/codes/epics-modules/asyn/asyn/`. Rust source root: `crates/asyn-rs/src/`.

## 결과 요약

| 분류 | 수 | 항목 |
|---|---|---|
| ✓ verified | 6 | Serial BREAK, Unix Domain Socket, disconnectOnReadTimeout, Interpose 4종, 양방향 notification, aai/aao |
| 🔄 partial | 9 | RS485, hostInfo, TCP server, asyn:READBACK, 초기값 동기화, lsi/lso/printf, ASYN_DESTRUCTIBLE, EOS atomic update, asynMask shift |
| ❌ invented | 4 | UInt64/UInt64Array, asynInt32Average/Float64Average + RingAverager, SO_REUSEPORT (server), ASYN_TRACE_STATE 비트 |
| ❌ wrong | 3 | TCP&/UDP&/UDP* 의미 swap, asynSetTrace*Mask 토큰 이름, FTDI scaffold config |

## ❌ WRONG (호환성 깨짐 — 최우선 수정)

### TCP&/UDP&/UDP* 프로토콜 의미 swap
- **C** (`drvAsynIPPort.c:360-387`): `tcp&`→TCP+SO_REUSEPORT, `udp&`→UDP+**SO_REUSEPORT** (NOT broadcast), `udp*`→UDP+**SO_BROADCAST** (NOT multicast), `udp*&`→UDP+broadcast+REUSEPORT
- **Rust** (`drivers/ip_port.rs:108-125`): `UDP&`→UdpBroadcast, `UDP*`→UdpMulticast — swap!. `UDP*&` 미구현
- **영향**: IOC startup script 호환성 — `drvAsynIPPortConfigure(.., "udp& ...")` 가 잘못된 socket flags 설정

### asynSetTrace*Mask 토큰 이름
- **C** (`asynShellCommands.c:670-849`): STARTSWITH macro 로 `ASYN_/TRACE_/TRACEIO_` prefix 스트립 → 짧은 이름 `ERROR/DEVICE/FILTER/DRIVER/FLOW/WARNING` (또는 full `ASYN_TRACEIO_DEVICE` 등). IO: `NODATA/ASCII/ESCAPE/HEX`. 구분자 `|` OR `+`. case-insensitive.
- **Rust** (`trace.rs:40-118`): 짧은 이름 `IO_DEVICE/IO_FILTER/IO_DRIVER` (C 와 다름). 구분자 `|` only. `NODATA` IO 토큰 부재. `STATE` (invented).
- **영향**: C 스타일 `"DEVICE+FLOW"` 입력 시 에러

### FTDI scaffold config shape
- **C** (`drvAsynFTDIPort.cpp:510-519`): `drvAsynFTDIPortConfigure(portName, vendor, product, baudrate, latency, priority, noAutoConnect, noProcessEos, mode)` — 9 positional iocshArgs
- **Rust** (`drivers/ftdi.rs`): `vid=0x..:pid=0x..:bitmode=mpsse` spec string. `baudrate/latency/mode` 부재
- **영향**: IOC startup 호환 안 됨, mode 의미 다름

## ❌ INVENTED (C asyn 에 부재)

### UInt64 / UInt64Array
- **C**: `find ~/codes/epics-modules/asyn -iname 'asynUInt64*'` = 빈 결과. `asynInt64.h`/`asynInt64Array.h` 만 존재. Issue #231 미머지.
- **Rust** (`interfaces/uint64.rs:13-27`): forward-looking trait

### asynInt32Average / Float64Average + RingAverager
- **C** (`devAsynInt32.c:99-100, 647-702`): 단순 `int sum; int numAverage;` 누적, threshold 시 `dval = sum/numAverage`, reset. **별도 interface 아님** — DTYP variant 만 (asynInt32 그대로 사용)
- **Rust** (`interfaces/average.rs:36-113`): 별도 trait + `RingAverager` (VecDeque drop-oldest) — **다른 알고리즘**
- 주석 "matching C asyn's drop-on-overflow policy" 도 잘못 (C 는 overflow 안 함)

### SO_REUSEPORT for IP server port
- **C** (`drvAsynIPServerPort.c:580-600`): server 측은 `[tcp|udp]` 토큰만, `SO_REUSEADDR` unconditional, **`SO_REUSEPORT` 토큰 없음**. PR #109 는 client `drvAsynIPPort` 한정.
- **Rust** (`drivers/ip_server_port.rs:139-142`): `SO_REUSEPORT/REUSEPORT` 토큰 파싱 + so_reuseport_allows_second_bind 테스트 — invented

### ASYN_TRACE_STATE 0x40 비트
- **C** (`asynDriver.h:211-216`): 6개 비트만 정의 — `ERROR=0x01, IO_DEVICE=0x02, IO_FILTER=0x04, IO_DRIVER=0x08, FLOW=0x10, WARNING=0x20`. `grep -rn "ASYN_TRACE_STATE" ~/codes/epics-modules/asyn` = 0 hits.
- **Rust** (`trace.rs:21-29`): `STATE = 0x40` + 회귀 테스트 — invented. PR #67 인용도 잘못

## 🔄 PARTIAL

### RS485 (PR #22)
- C 5개 setOption 키 / Rust 3개 wired (delay 옵션 부재)
- C `struct serial_rs485 { __u32 flags; __u32 delay_rts_before_send; __u32 delay_rts_after_send; __u32 padding[5]; }` / Rust 단순 `c_ulong` flags word → Linux 커널에서 silent fail 가능
- C `getOption("rs485_*")` (line 209-225) 부재

### hostInfo runtime (Issue #12)
- C `parseHostInfo` (`:273-401`): 기존 socket close + 전체 config 재파싱 (protocol, FLAG_BROADCAST, FLAG_SO_REUSEPORT, FLAG_SHUTDOWN re-arm, CLOSE_SOCKET_DELAY)
- Rust (`ip_port.rs:700-712`): host/port/local_port 만 갱신 → 런타임 transport 전환 안 됨

### TCP server mode (PR #148/#109)
- C (`drvAsynIPServerPort.c:681-708`): pre-create child asyn ports `parent:0/parent:1/...` via `drvAsynIPPortConfigure` (noAutoConnect=1)
- Rust (`ip_server_port.rs:223,504`): 단일 슬롯 테이블, child port 등록 안 함 → 외부 dev support 가 client 를 port 이름으로 addressing 불가
- C `MAX_NUM_CLIENTS=4` literal 사실 부재 (사용자 explicit 필수); 우리 주석 (`:57`) 잘못

### asyn:READBACK info-tag (PR #60/#208)
- C 6개 device support 모두 init 시 `asynDbGetInfo(pr, "asyn:READBACK")` 자동 호출
- Rust (`adapter.rs:181,247-258,711`): flag/gating OK, info-tag 자동 파싱 부재 (수동 `set_asyn_readback` 필요). epics-rs db_loader 가 info-tag 캡처해서 record 에 노출하는지 확인 필요

### 초기값 동기화 (Issue #24/PR #27)
- C output 레코드만 init sync read (initAo, initBo, initLongout, initMbbo 등). devAsynOctet output 도 안 함
- Rust (`adapter.rs:837-842`): output + **input** 까지 enable → over-apply. 주석 "Matches C EPICS devAsynXxx init_common() behavior" 잘못

### lsi / lso / printf (PR #104)
- C (`devAsynOctet.c:52-54, 177, 1097, 1131, 1149`): lsiRecord/lsoRecord/printfRecord 명시 init, `pPvt->pLen` 가 long-string len 필드 (\0 counted)
- Rust (`adapter.rs:380,411,462`): stringin/stringout/waveform 만, lsi/lso/printf 부재. 256-byte 고정 버퍼 (C 는 sizv-driven)

### ASYN_DESTRUCTIBLE (PR #171)
- C (`asynDriver.h:97 = 0x0004`, `asynManager.c:2251-2308`): `enabled=FALSE/defunct=TRUE/NULL drvPvt on interfaces/asynExceptionShutdown broadcast`. ASYN_DESTRUCTIBLE flag opt-in.
- Rust (`port.rs:75-76,84,861`): `PortFlags::destructible` + no-op `shutdown()` trait method. lifecycle 부재. default `true` 도 C 와 다름 (C 는 opt-in)

### EOS atomic update (Issue #103)
- C: `asynInterposeEos.c` 에 mutex 부재. `asynOctetSyncIO.c:300-321,346-367` 의 `lockPort/unlockPort` 가 외부 직렬화. Issue #103 의 핵심은 setEos 가 IOC init/exit 에서 connect-wait blocking
- Rust: `Arc<Mutex<dyn PortDriver>>` (port.rs:493) 가 등가 직렬화 제공. set_input_eos/set_output_eos 가 in-memory only — connect-wait 안 함 → 실제 #103 증상 자체가 reproduce 안 됨. 미래에 connect-gated EOS 도입시 #103 패턴 재현 안 하도록 명문화 필요.

### asynMask shift (Issue #166)
- C (`devAsynUInt32Digital.c:199,627,881-1089`): link 은 mask 만 (shift 없음), `computeShift(mask)` 로 mask 의 low-bit 위치 derive → mbbiDirect/mbboDirect/mbbi/mbbo 의 `pr->shft` set, read 시 `value >>= shft` 적용
- Rust (`adapter.rs:95-146,235,381,481`): mask 만 적용, shift derivation/적용 부재 → 다중-비트 read 가 raw masked value (right-align 안 됨)

---

## 수정 작업 우선순위

1. **WRONG** (3) — 호환성 회복 최우선
2. **INVENTED** (4) — 제거 또는 "Rust extension" 명시
3. **PARTIAL** (9) — 점진적 보완

각 항목별 Task ID:
- W1 #19: TCP&/UDP&/UDP* swap fix
- W2 #20: asynSetTrace*Mask 토큰 이름
- W3 #21: FTDI 9 positional iocshArg
- I1 #22: UInt64/UInt64Array — invented 표기 또는 제거
- I2 #23: Average algorithm fix (sum+numAverage)
- I3 #24: SO_REUSEPORT server 토큰 제거
- I4 #25: ASYN_TRACE_STATE 0x40 비트 제거
- P1 #26: RS485 5 키 + struct serial_rs485 + getOption
- P2 #27: hostInfo protocol 갱신
- P3 #28: TCP server child port 모델
- P4 #29: asyn:READBACK info-tag 자동 인식
- P5 #30: 초기값 동기화 input 제거
- P6 #31: lsi/lso/printf 어댑터
- P7 #32: ASYN_DESTRUCTIBLE shutdown lifecycle
- P8 #33: EOS connect-wait 정책
- P9 #34: asynMask shift computeShift + right-align
