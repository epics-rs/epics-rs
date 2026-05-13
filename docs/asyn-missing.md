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

| 항목 | C asyn 출처 | 비고 |
|---|---|---|
| **USBTMC** (`drvAsynUSBTMC`) | `drvAsynUSBTMC.c` | iocshArg: `(portName, vendorId int, productId int, serialNumber*, priority int, flags int)`. libusb 기반 (`libusb_init`/`libusb_open_device_with_vid_pid`). bulk OUT/IN BTAG/EOM 프레이밍. |
| **VXI-11** (`drvVxi11`) | `vxi11/drvVxi11.c` | iocshArg: `(dn, hostName, flags, vxiName, ...)`. ONC RPC (Sun RPC `clnt_create`). create_link / device_write / device_read / device_clear / destroy_link / abort 채널. |
| **HiSLIP** | Issue #130 (미머지) | C asyn 에 코드 자체 없음 — 공식 driver 가 아직 추가되지 않은 상태. 추가하려면 IVI-6.1 spec 기반 신규 구현 필요. |

### 2. 어댑터 / 레코드 계층 누락

| 항목 | C asyn 출처 | 비고 |
|---|---|---|
| `asyn:FIFO` info-tag 링 버퍼 | `devEpics/devAsynInt32.c::createRingBuffer` 등 | per-record `ringBuffer[ringSize+1]` (default `ringSize = DEFAULT_RING_BUFFER_SIZE = 10`, `atoi(asynDbGetInfo(pr, "asyn:FIFO"))` overrides). drop-oldest on overflow + `ringBufferOverflows++`. **`scanIoRequest` 는 새 entry 추가시에만 호출 — overflow 로 overwrite 할 때는 호출 안 함** (process queue flooding 방지). 해당 디바이스 서포트 6종 모두 (Int32, Int64, Float64, Octet, UInt32Digital, XXXArray) 같은 패턴. |
| `getBounds` ↔ ai/ao LINEAR ESLO/EOFF | `devEpics/devAsynInt32.c::initAi` 등 | C asyn 의 `getBounds` 는 asynInt32/asynInt64 만 보유 (`asynInt32.h` L36). `(int* low, int* high)` 시그니처. `devAsynInt32::initAi/initAiAverage` 가 init 시 `pasynInt32SyncIO->getBounds` 호출 → `pPvt->deviceLow/deviceHigh` 저장 → `convertAi` 가 LINR=LINEAR 모드에서 ESLO/EOFF 계산 사용. **DRVL/DRVH/HOPR/LOPR 는 set 하지 않음** (사용자 .db 필드). asyn-rs 의 `port.rs::get_bounds_int32`/`int64` 는 trait 에 존재하나 adapter 측 LINEAR convert 호출 사이트 미존재. |
| `getLimits` 인터페이스 | (Issue #218 미머지) | C asyn 에 `getLimits` 는 존재 자체가 없음 — Issue #218 은 feature request 이며 머지되지 않음. asynFloat64 에는 `getBounds` 도 없음. |

### 3. 아키텍처

| 항목 | C asyn 출처 | 비고 |
|---|---|---|
| `asynParamSet` 파라미터 그룹 | `asynPortDriver/asynParamSet.h` | C++ 클래스: `std::vector<asynParam{name, type, int*}>` + `add()` + `getParamDefinitions()`. asynPortDriver 생성자가 받아서 createParam 일괄 호출. **평탄 리스트 (트리 아님).** |
| **PVI** (PVInterface) | (없음) | C asyn 에 PVI / 트리 구조 파라미터 토폴로지는 존재하지 않음. PR 후보 단계도 확인 안 됨. |

---

## 다음 우선순위 (C asyn 소스 대비 정확 구현)

1. **`asyn:FIFO` adapter wiring** — `devAsynInt32::createRingBuffer` 패턴 (default 10, atoi only, overflow 시 scanIoRequest 안 함) — medium
2. **`getBounds_int32/int64` ↔ LINEAR ESLO/EOFF** — `devAsynInt32::initAi`/`convertAi` 와 동일한 init→deviceLow/High→convert 흐름 — medium
5. **USBTMC driver** — libusb-rs (rusb/nusb) + iocshArg-style config (vid, pid, serial, priority, flags) + BTAG/EOM 프레이밍 — large
6. **VXI-11 driver** — ONC RPC (`onc-rpc` crate 후보) + iocshArg-style config (dn, hostName, flags, vxiName) — large
7. **HiSLIP driver** — C asyn 에 없음, IVI-6.1 spec 기반 신규 — large
8. **`asynParamSet` 평탄 그룹 헬퍼** — `vector<asynParam>` 등가 — small (PVI 는 C asyn 에 부재 → 미구현 항목 아님)

---

### 종합 요약
`asyn-rs`는 단순한 포팅을 넘어 C++ 버전의 고질적 버그와 최신 요구사항(TCP 비동기 커넥션, RS485 지원, SO_REUSEPORT, BREAK, AVERAGE 등)을 이미 매우 높은 수준으로 선제 반영. 본 audit 에서 이전 세션의 일부 invented scaffold (USBTMC/Prologix/VXI-11/HiSLIP scaffold, `getLimits` trait, PVI ParamTree, UDP-server peer-routing, A1-A3 wiring) 가 C asyn 소스 대비 검증 실패로 revert. 남은 작업은 C asyn 원본 시그니처/세만틱 대비 정확한 구현으로 다시 접근.
