# epics-rs (asyn-rs) 미구현 기능 전수 분석 보고서

C++ `epics-modules/asyn`의 2019년 이후 모든 주요 Commit, Issue, PR을 전수 분석하여, 현재 `epics-rs` (`crates/asyn-rs`) 구현체와 대조한 결과입니다. 이 문서는 코드 베이스와 대조해 verify된 결과만 기록 — 추정/희망 verdict 금지.

먼저, `epics-rs`는 놀랍게도 최근 C++ `asyn`에 추가된 많은 최신 기능들과 이슈 수정 사항들을 이미 꼼꼼하게 반영하고 있습니다.

## 이미 구현 (verified ✓)

| 항목 | C asyn 출처 | 위치 |
|---|---|---|
| `UInt64`/`UInt64Array` 인터페이스 | Issue #231 | `interfaces/uint64.rs` |
| RS485 시리얼 지원 | PR #22 | `drivers/serial_port.rs` |
| Serial BREAK 전송 (`send_break`) | PR #188 | `drivers/serial_port.rs::send_break` |
| TCP `TCP&` (비동기 연결) / UDP `UDP&` (broadcast) / `UDP*` (multicast) | PR #109 | `drivers/ip_port.rs::IpProtocol` |
| `SO_REUSEPORT` 지원 | PR #109 | `drivers/ip_server_port.rs::IpServerConfig::reuse_port` |
| Unix Domain Socket | Issue #31 | `drivers/ip_port.rs::IpProtocol::Unix` |
| `disconnectOnReadTimeout` | PR #6 | `drivers/ip_port.rs::IpPortConfig::disconnect_on_read_timeout` |
| 런타임 `hostInfo` (`set_option("hostInfo")`) | Issue #12 | `drivers/ip_port.rs` (set_option) |
| Interpose 필터 (`asynInterposeDelay`/`Echo`/`Eos`/`Flush`) | PR #79 | `interpose/{delay,echo,eos,flush}.rs` |
| TCP 서버 모드 (`drvAsynIPServerPort`) | PR #148/#109 | `drivers/ip_server_port.rs` |
| UDP 서버 모드 (`drvAsynIPServerPort UDP`) | `drvAsynIPServerPort.c` SOCK_DGRAM | `drivers/ip_server_port.rs::IpServerProtocol::Udp` — single shared `UdpCache` (C parity, source addr 무시), recv 워커 스레드 (cache empty 일 때만 recv), `read_octet` 가 cache drain 후 0 반환 (non-blocking, C parity), `write_octet` 가 read-only 에러. UDP_MAX_DATAGRAM=65507. 4× 회귀 테스트. |
| `ASYN_TRACE_STATE` 마스크 비트 | PR #67 | `trace.rs::TraceMask::STATE` |
| `asynSetTrace*Mask` 문자열 파싱 | PR #76 | `trace.rs::*::from_symbolic` |
| `asynInt32Average`/`asynFloat64Average` + `RingAverager` | Issue #30 | `interfaces/average.rs` |
| `asyn:READBACK` info-tag | PR #60 / #208 | `adapter.rs::asyn_readback` field + auto-detect |
| 초기값 동기화 (initial readback) | Issue #24 / PR #27 | `adapter.rs::with_initial_readback` |
| `lsi`/`lso`/`printf` 어댑터 매핑 | PR #104 | `adapter.rs::asynOctet` 경로가 String/CharArray ↔ EpicsValue 변환 처리 |
| `ASYN_DESTRUCTIBLE` / shutdown | PR #171 | `port.rs::PortFlags::destructible` + `PortDriver::shutdown` |
| FTDI 드라이버 스캐폴드 | PR #88 | `drivers/ftdi.rs` (config parser + scaffold, hardware path feature-gated) |
| Prologix GPIB 드라이버 | `drvPrologixGPIB.c` | `drivers/prologix.rs` — embedded `DrvAsynIPPort` (`<port>_TCP`), per-write `setAddress(user.addr)` (addr/100 primary + addr%100+96 secondary), on-connect 8-line init burst (`++savecfg`/`++mode`/`++ifc`/`++eos`/`++eoi`/`++eot_char`/`++eot_enable`/`++ver`) + version-string capture, char escaping (`\r`/`\n`/`\033`/`+` → `\033`-prefix), EOS append + `\n` terminator, `++read eoi`/`++read <eos>` flow with EOT-marker strip + binary-mode disambiguation. 5× loopback 회귀 테스트. |
| 양방향 파라미터 notification | Issue #46 | `interrupt.rs::Interrupt` + `call_param_callbacks` |
| EOS 설정자 atomic update | Issue #103 | `interpose/eos.rs` Mutex-protected |
| `asynMask` shift | Issue #166 | record-layer SHFT (mbbiDirect/mbboDirect) — asyn-side mask는 bit selection만 |
| `aai`/`aao` 레코드 어댑터 매핑 | PR #162 | `epics-base-rs` `WaveformRecord::with_kind(Aai/Aao)` + `asyn-rs::adapter::normalize_asyn_dtyp` (asynFloat64ArrayIn/Out → asynFloat64Array). `dtyp_normalize_aai_aao_array_in_out` 회귀 테스트로 fence |

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
