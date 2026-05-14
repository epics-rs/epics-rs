# epics-rs (asyn-rs) 미구현 기능 전수 분석 보고서

C++ `epics-modules/asyn`의 2019년 이후 모든 주요 Commit, Issue, PR을 전수 분석하여, 현재 `epics-rs` (`crates/asyn-rs`) 구현체와 대조한 결과입니다. 이 문서는 코드 베이스와 대조해 verify된 결과만 기록 — 추정/희망 verdict 금지.

먼저, `epics-rs`는 놀랍게도 최근 C++ `asyn`에 추가된 많은 최신 기능들과 이슈 수정 사항들을 이미 꼼꼼하게 반영하고 있습니다.

## 이미 구현 (verified ✓ — 2026-05-14 audit 반영)

| 항목 | C asyn 출처 | 위치 |
|---|---|---|
| RS485 시리얼 지원 (5 키 + struct serial_rs485 + getOption) | PR #22 | `drivers/serial_port.rs` (audit P1, commit `38e7743`) |
| Serial BREAK 전송 (`send_break`) | PR #188 | `drivers/serial_port.rs::send_break` |
| TCP `TCP&` / UDP `UDP&` (REUSEPORT) / `UDP*` (broadcast) / `UDP*&` | PR #109 + audit W1 fix | `drivers/ip_port.rs::IpProtocol` (audit W1 swap fix, commit `9ff5659`) |
| Unix Domain Socket | Issue #31 | `drivers/ip_port.rs::IpProtocol::Unix` |
| `disconnectOnReadTimeout` | PR #6 | `drivers/ip_port.rs::IpPortConfig::disconnect_on_read_timeout` |
| 런타임 `hostInfo` 전체 reparse + 20ms close delay | Issue #12 | `drivers/ip_port.rs::set_option("hostInfo")` (audit P2, commit `40fa1d0`) |
| Interpose 필터 (`asynInterposeDelay`/`Echo`/`Eos`/`Flush`) | PR #79 | `interpose/{delay,echo,eos,flush}.rs` |
| TCP 서버 모드 + child port (`parent:N`) | PR #148/#109 | `drivers/ip_server_port.rs::DrvAsynIPSubport` (audit P3, commit `1e2716a`) |
| UDP 서버 모드 (`drvAsynIPServerPort UDP`) | `drvAsynIPServerPort.c` SOCK_DGRAM | `drivers/ip_server_port.rs::IpServerProtocol::Udp` — single shared `UdpCache` (C parity, source addr 무시), recv 워커 스레드 (cache empty 일 때만 recv), `read_octet` 가 cache drain 후 0 반환 (non-blocking, C parity), `write_octet` 가 read-only 에러. UDP_MAX_DATAGRAM=65507. |
| `asynSetTrace*Mask` 문자열 파싱 (C 토큰 이름) | PR #76 + audit W2 fix | `trace.rs::*::from_symbolic` (audit W2, commit `9691605`) |
| `asynInt32Average`/`asynFloat64Average` DTYP — `SumAverager` (C `sum`+`numAverage`) | Issue #30 + audit I2 fix | `interfaces/average.rs::SumAverager` (audit I2, commit `7befd0e`) |
| `asyn:READBACK` info-tag 자동 인식 + `asyn:INITIAL_READBACK` | PR #60 / #208 | `adapter.rs::apply_record_info` (audit P4, commit `f2370af`) |
| 초기값 동기화 — output 한정 (input 제거) | Issue #24 / PR #27 + audit P5 fix | `adapter.rs::universal_asyn_factory` (audit P5, commit `4b6e2f7`) |
| `lsi`/`lso`/`printf` SIZV-driven asynOctet 버퍼 | PR #104 + audit P6 fix | `adapter.rs::octet_max_size` (audit P6, commit `55dc8fd`) |
| `ASYN_DESTRUCTIBLE` shutdown lifecycle (opt-in, default false) | PR #171 + audit P7 | `port.rs::shutdown_lifecycle` + `manager.rs::shutdown_port` (audit P7, commit `a20aede`) |
| FTDI 드라이버 스캐폴드 (9 positional iocshArg) | PR #88 + audit W3 fix | `drivers/ftdi.rs` (audit W3, commit `5d2253c`) |
| Prologix GPIB 드라이버 | `drvPrologixGPIB.c` | `drivers/prologix.rs` |
| 양방향 파라미터 notification | Issue #46 | `interrupt.rs::Interrupt` + `call_param_callbacks` |
| EOS 설정자 atomic update (Rust는 in-memory only — issue #103 reproduce 불가) | Issue #103 | `interpose/eos.rs` + `port.rs` EOS section (audit P8, commit `726ddba`) |
| `asynMask` shift — `computeShift(mask)` + MASK/SHFT 자동 전파 | Issue #166 + audit P9 fix | `adapter.rs::compute_mask_shift` + `apply_linear_eslo_eoff` (audit P9, commit `e96561b`) |
| `aai`/`aao` 레코드 어댑터 매핑 | PR #162 | `epics-base-rs` `WaveformRecord::with_kind` + `adapter.rs::normalize_asyn_dtyp` |
| `asyn:FIFO` 링 버퍼 (DEFAULT=10, atoi override, scanIoRequest only-on-fresh-add) | `devAsynInt32::createRingBuffer` | `adapter.rs::InterruptFifo` (missing M1, commit `04ef574`) |
| `getBounds_int32/int64` ↔ ai LINEAR ESLO/EOFF | `devAsynInt32::initAi`/`convertAi` | `adapter.rs::apply_linear_eslo_eoff` (missing M2, commit `614e7eb`) |
| `asynParamSet` 평탄 그룹 helper | `asynPortDriver/asynParamSet.h` | `param.rs::AsynParamSet` (missing M3, commit `5817fad`) |
| USBTMC 드라이버 스캐폴드 (6 positional iocshArg + BTAG 프레이밍) | `drvAsynUSBTMC.c` | `drivers/usbtmc.rs` (missing M4, commit `448724e`) |
| VXI-11 드라이버 스캐폴드 (7 positional iocshArg + RPC 상수) | `vxi11/drvVxi11.c` | `drivers/vxi11.rs` (missing M5, commit `2b67092`) |

### Rust extensions (C asyn 부재 — 명시)

| 항목 | C asyn 상태 | 위치 |
|---|---|---|
| `UInt64`/`UInt64Array` 인터페이스 | Issue #231 unmerged — C asyn 부재 | `interfaces/uint64.rs` — module docstring 에 Rust extension 표기 (audit I1, commit `2bbbb45`) |
| `ASYN_TRACE_STATE` 마스크 비트 | C `asynDriver.h:211-216` 에 6비트만 존재 | audit I4 에서 제거 (commit `9691605`) |
| `SO_REUSEPORT` server 토큰 | C `drvAsynIPServerPort.c` server 측은 `[tcp\|udp]`만 | audit I3 에서 제거 (commit `1ec01f3`) |

## 진짜 미구현 (verified gaps — C asyn 소스 대비)

### 1. 하드웨어 계측기 통신 프로토콜 드라이버

| 항목 | C asyn 출처 | 상태 | Commit |
|---|---|---|---|
| **USBTMC** (`drvAsynUSBTMC`) | `drvAsynUSBTMC.c` | ✅ scaffold | `448724e` (M4) |
| **VXI-11** (`drvVxi11`) | `vxi11/drvVxi11.c` | ✅ scaffold | `2b67092` (M5) |
| **HiSLIP** | Issue #130 (미머지) | ⛔ skip — C asyn 에 코드 자체 없음. invented 가 됨 |

USBTMC/VXI-11은 FTDI 와 동일한 scaffold 컨벤션 — iocshArg 위치 인자 + 프로토콜 상수 (BULK_IO_HEADER_SIZE=12, MESSAGE_ID 1/2, BTAG 0xFF→1 wrap, pad4 / DEVICE_CORE=0x0607AF v1, FLAG bits 0x1/0x2/0x4, link-kind heuristic) + Cargo feature gate (`usbtmc` / `vxi11`). HW path (nusb/rusb / onc-rpc) 는 별도 PR 로 후속.

### 2. 어댑터 / 레코드 계층

| 항목 | C asyn 출처 | 상태 | Commit |
|---|---|---|---|
| `asyn:FIFO` info-tag 링 버퍼 | `devEpics/devAsynInt32.c::createRingBuffer` 등 | ✅ done | `04ef574` (M1) |
| `getBounds` ↔ ai LINEAR ESLO/EOFF | `devEpics/devAsynInt32.c::initAi`/`convertAi` | ✅ done | `614e7eb` (M2) |
| `getLimits` 인터페이스 | (Issue #218 미머지) | ⛔ skip — C asyn 에 존재 자체 없음 |

### 3. 아키텍처

| 항목 | C asyn 출처 | 상태 | Commit |
|---|---|---|---|
| `asynParamSet` 평탄 그룹 헬퍼 | `asynPortDriver/asynParamSet.h` | ✅ done | `5817fad` (M3) |
| **PVI** (PVInterface) | (없음) | ⛔ skip — C asyn 부재 |

---

### 종합 요약 (2026-05-14 update)

5/8 우선순위 항목 완료 — `M1` asyn:FIFO, `M2` LINEAR ESLO/EOFF, `M3` AsynParamSet, `M4` USBTMC scaffold, `M5` VXI-11 scaffold. 모두 C asyn 원본 시그니처/세만틱 / 상수에 1:1 매핑됨. 워크스페이스 nextest 3485/3485 통과.

남은 3 항목 (HiSLIP, getLimits, PVI)은 모두 C asyn 부재 — 추가 구현은 invention 으로 분류되므로 본 audit 의 scope 밖. M4/M5 의 HW path 활성화는 use case 확정 후 별도 PR.
