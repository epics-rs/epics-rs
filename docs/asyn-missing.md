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
| TCP 서버 모드 (`drvAsynIPServerPort`) | PR #148/#109 | `drivers/ip_server_port.rs` (this session) |
| `ASYN_TRACE_STATE` 마스크 비트 | PR #67 | `trace.rs::TraceMask::STATE` (this session) |
| `asynSetTrace*Mask` 문자열 파싱 | PR #76 | `trace.rs::*::from_symbolic` (this session) |
| `asynInt32Average`/`asynFloat64Average` + `RingAverager` | Issue #30 | `interfaces/average.rs` (this session) |
| `asyn:READBACK` info-tag | PR #60 / #208 | `adapter.rs::asyn_readback` field + auto-detect at L658 |
| 초기값 동기화 (initial readback) | Issue #24 / PR #27 | `adapter.rs::with_initial_readback` (L242) |
| `lsi`/`lso`/`printf` 어댑터 매핑 | PR #104 | `adapter.rs::asynOctet` 경로가 String/CharArray ↔ EpicsValue 변환 처리 |
| `ASYN_DESTRUCTIBLE` / shutdown | PR #171 | `port.rs::PortFlags::destructible` + `PortDriver::shutdown` (trait L860) |
| FTDI 드라이버 스캐폴드 | PR #88 | `drivers/ftdi.rs` (config parser + scaffold, hardware path feature-gated) |
| USBTMC 드라이버 스캐폴드 | 기존 | `drivers/usbtmc.rs` (this session — vid/pid/serial/interface parser + scaffold, `usbtmc-hw` feature-gated) |
| Prologix GPIB 드라이버 스캐폴드 | PR #129 | `drivers/prologix.rs` (this session — host:port + GPIB addr parser, `++addr` line builder, TCP path delegates to ip_port) |
| UDP 서버 모드 (`drvAsynIPServerPort UDP`) | 기존 | `drivers/ip_server_port.rs::IpServerProtocol::Udp` (this session — parser + protocol enum; runtime listener returns "not yet wired" pending UDP-server adapter) |
| `asyn:FIFO` info-tag 링 버퍼 | 2015년경 | `asyn_record/fifo.rs` (this session — `RingBuffer<T>` drop-oldest + overrun counter + `parse_fifo_tag`) |
| `getLimits` 인터페이스 | Issue #218 | `interfaces/limits.rs::AsynLimits` (this session — `IntLimits`/`FloatLimits` + read trait) |
| 양방향 파라미터 notification | Issue #46 | `interrupt.rs::Interrupt` + `call_param_callbacks` (verified ALREADY) |
| EOS 설정자 atomic update | Issue #103 | `interpose/eos.rs` Mutex-protected |
| `asynMask` shift | Issue #166 | record-layer SHFT (mbbiDirect/mbboDirect) — asyn-side mask는 bit selection만 |

## 진짜 미구현 (verified gaps)

### 1. 하드웨어 계측기 통신 프로토콜 드라이버

| 항목 | C asyn 출처 | 상태 | 비고 |
|---|---|---|---|
| **USBTMC** (`drvAsynUSBTMC`) | 기존 | 🔄 PARTIAL | `drivers/usbtmc.rs` 스캐폴드 (config parser + `PortDriver` trait) 완료. `usbtmc-hw` feature 활성화 시 `rusb`/`nusb` 바인딩 wiring 필요 |
| **Prologix GPIB** | PR #129 | 🔄 PARTIAL | `drivers/prologix.rs` 스캐폴드 (host:port + GPIB addr parser + `++addr` line builder) 완료. `connect()` 가 ip_port adapter 위임 — TCP 본체는 follow-up |
| **VXI-11** (`drvVxi11`) | 기존 | ❌ 미구현 | ONC RPC 클라이언트 필요 (`onc-rpc` crate). 1000+ LOC |
| **HiSLIP** | Issue #130 | ❌ 미구현 | TCP 기반 고속 LAN 계측기 프로토콜. 500-800 LOC |

### 2. 어댑터 계층 누락

| 항목 | C asyn 출처 | 상태 | 비고 |
|---|---|---|---|
| `aai`/`aao` 레코드 어댑터 매핑 | PR #162 | 🔄 PARTIAL | record 자체는 구현 (`server/records/waveform.rs` SubArray/Aai/Aao kinds). asyn-rs adapter는 `asynInt32Array`/`asynFloat64Array` 인터페이스로 처리 — 별도 record-type 명시 매핑은 아직 |
| `asyn:FIFO` info-tag 링 버퍼 | 2015년경 | 🔄 PARTIAL | `asyn_record/fifo.rs::RingBuffer<T>` (drop-oldest + overrun counter + tag parser) 완료. record adapter 측 push/pop 호출 사이트 wiring 은 follow-up |
| `getLimits` 인터페이스 | Issue #218 | 🔄 PARTIAL | `interfaces/limits.rs::AsynLimits` trait + `IntLimits`/`FloatLimits` 완료. record-layer DRVH/DRVL ↔ limits read 자동 동기화는 follow-up |

### 3. 아키텍처

| 항목 | C asyn 출처 | 상태 | 비고 |
|---|---|---|---|
| **PVI** (PVInterface) tree-structured params | PR #117 | ❌ 미구현 | 계층적 파라미터 토폴로지. 우리는 flat `ParamSet` 만 보유 |
| `asynParamSet` 그룹화 클래스 | PR #117 | ❌ 미구현 | 대규모 파라미터 논리적 그룹/검색. 우리는 단일 namespace |
| UDP 서버 모드 (`drvAsynIPServerPort UDP`) | 기존 | 🔄 PARTIAL | `drivers/ip_server_port.rs` 가 `IpServerProtocol::Udp` 파싱/저장. 런타임 `open_listener` 는 명시적 "not yet wired" 반환 — UDP-server adapter (peer-per-datagram slot 매핑) 가 다음 단계 |

### 4. 외부-dep 필요 항목 묶음

- USBTMC (`usbtmc-hw`), FTDI (`ftdi-mpsse`) hardware-path —
  scaffold-only-status. 패턴: config parser + `PortDriver` trait
  impl + 외부 dep feature-gate.
- Prologix GPIB — 외부 dep 없음 (TCP). scaffold + `++addr` 빌더
  완료, `connect()` 가 ip_port adapter 위임 (follow-up).
- VXI-11, HiSLIP — 아직 dep crate 선정 단계 (`onc-rpc` 후보).

---

## 다음 우선순위 (구현 가능 순)

1. **UDP server 런타임 어댑터** (peer-per-datagram slot 매핑) — small/medium
2. **getLimits ↔ record DRVH/DRVL 자동 동기화** (adapter wiring) — small
3. **asyn:FIFO record adapter wiring** (push/pop 호출 사이트) — medium
4. **USBTMC `usbtmc-hw` rusb/nusb 바인딩** — medium
5. **Prologix `connect()` ip_port 위임 wiring** — small
6. **aai/aao record-type explicit mapping** (adapter type switch) — medium
7. **PVI / asynParamSet** (architectural, multi-day) — large
8. **VXI-11 / HiSLIP** (RPC / TCP protocols, multi-day each) — large

---

### 종합 요약
`asyn-rs`는 단순한 포팅을 넘어 C++ 버전의 고질적 버그와 최신 요구사항(TCP 비동기 커넥션, RS485 지원, SO_REUSEPORT, BREAK, AVERAGE 등)을 이미 매우 높은 수준으로 선제 반영. 본 세션에서 USBTMC/Prologix scaffold, UDP-server protocol parser, `asyn:FIFO` ring buffer, `getLimits` trait 까지 추가 — 남은 큰 마일스톤은:

1. **계측기 전용 프로토콜 (VXI-11, HiSLIP)** — RPC/TCP 본체 구현
2. **scaffold → 런타임 wiring** (USBTMC HW, Prologix ip_port 위임, FIFO record-adapter, UDP-server adapter, getLimits ↔ DRVH/DRVL)
3. **PVI tree topology** — 아키텍처적 확장
