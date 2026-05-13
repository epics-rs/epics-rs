# epics-rs (asyn-rs) 미구현 기능 전수 분석 보고서

C++ `epics-modules/asyn`의 2019년 이후 모든 주요 Commit, Issue, PR을 전수 분석하여, 현재 `epics-rs` (`crates/asyn-rs`) 구현체와 대조한 결과입니다.

먼저, `epics-rs`는 놀랍게도 최근 C++ `asyn`에 추가된 많은 최신 기능들과 이슈 수정 사항들을 이미 꼼꼼하게 반영하고 있습니다.
*   **이미 반영된 최신 기능들 (구현 완료)**:
    *   `UInt64` 및 `UInt64Array` 지원 (asyn Issue #231)
    *   RS485 지원 및 Serial Break 전송 (PR #22, PR #188)
    *   TCP 비동기 연결(`TCP&`), UDP 브로드캐스트/멀티캐스트 및 `SO_REUSEPORT` 지원 (PR #109)
    *   Unix Domain Socket 연결 지원 (Issue #31)
    *   Read Timeout 시 자동 연결 해제 (`disconnectOnReadTimeout`) (PR #6)
    *   런타임 IP 변경을 위한 `hostInfo` 옵션 (Issue #12)
    *   `asynInterposeDelay`, `asynInterposeEcho`, `asynInterposeEos` 등의 Interpose 필터 (PR #79)

위 기능들을 제외하고, 현재 `epics-rs`에 **실제로 누락되어 있거나 부분적으로만 구현된 핵심 기능**은 다음과 같습니다.

## 1. 하드웨어 계측기 통신 프로토콜 및 드라이버

현재 `asyn-rs`는 IP와 Serial 포트에 집중되어 있으며, 특수 목적의 계측기 버스 드라이버들이 아직 이식되지 않았습니다.

*   **USBTMC (`drvAsynUSBTMC`)**: USB 기반 계측기 제어 프로토콜. (libusb 기반)
*   **VXI-11 (`drvVxi11`)**: 기존 이더넷 계측기 제어 표준. (RPC 기반)
*   **HiSLIP (High-Speed LAN Instrument Protocol)**: VXI-11의 성능 한계를 극복하기 위해 제정된 TCP 기반 고속 프로토콜. (Issue #130)
*   **FTDI/SPI (`drvAsynFTDIPort`)**: FTDI 칩셋을 활용한 SPI/I2C 버스 제어. (PR #88)
*   **Prologix GPIB (`drvPrologixGPIB`)**: 이더넷-GPIB 컨버터 지원. (PR #129)

## 2. EPICS 레코드 어댑터 (Device Support) 관련 누락

`asyn` 포트와 EPICS IOC 레코드를 연결해주는 어댑터 계층(`asyn_record.rs` 및 `adapter`)에서 최신 `asyn`의 추가 기능들이 일부 누락되어 있습니다.

*   **배열 및 특수 문자열 레코드 지원**:
    *   `aai` (Array Action Input) 및 `aao` (Array Action Output) 레코드 지원. (PR #162)
    *   `lsi` (Long String Input), `lso` (Long String Output), `printf` 레코드 지원. (PR #104)
*   **`asyn:READBACK` Info Tag 핸들링**: Output 레코드에 대해 드라이버 콜백이 값을 업데이트할 때 발생하는 데드락 및 UDF(Undefined) 상태 이상을 방지하는 특수 로직. (PR #60, Issue #136)
*   **초기값 동기화 (Initial Value)**: `devAsynOctet` 기반 Output 레코드 등이 `init_record` 단계에서 드라이버로부터 초기 상태를 읽어오는(Read-back) 기능. (Issue #24, PR #27)

## 3. 포트 소멸자 및 런타임 제어 (Destructible Ports)

최근 C++ `asyn`은 런타임에 포트 인터페이스를 안전하게 제거하는 기능들을 아키텍처 레벨에서 도입했습니다.
*   **`ASYN_DESTRUCTIBLE` / 포트 명시적 종료**: `shutdownPort()`, `shutdownPortDriver()`를 통해 실행 중인 폴링 스레드와 콜백을 즉시 멈추고 리소스를 반환하는 기능. (PR #171)
    *   *분석*: `asyn-rs`는 Rust의 소유권(Drop) 모델을 통해 Actor 채널이 닫히면 스레드가 정리되도록 설계되어 있으나, C++ `asyn`처럼 강제로 포트 인터페이스 연결을 끊거나 `findInterface`를 거부하는 명시적인 상태 관리 로직(defunct 상태)은 완전히 매핑되어 있지 않습니다.

## 4. 기타 아키텍처 및 유틸리티

*   **PVI (PVInterface) 지원**: 트리 구조의 파라미터 토폴로지 지원. (PR #117)
*   **`asynParamSet`**: 대규모 파라미터를 다룰 때 파라미터 집합을 논리적으로 관리하고 검색하기 위한 클래스. (PR #117)
*   **`getLimits` 인터페이스**: 파라미터의 타입뿐만 아니라 설정 가능한 최소/최대 한계값을 드라이버에 질의하는 기능. (Issue #218)

---
### 요약
`epics-rs/crates/asyn-rs`는 단순한 포팅을 넘어 C++ 버전의 고질적 버그와 최신 요구사항(TCP 비동기, RS485 등)을 이미 매우 높은 수준으로 선제 반영하고 있습니다. 앞으로 구현해야 할 주요 마일스톤은 **계측기 전용 프로토콜(USBTMC, VXI-11)의 추가**와 **최신 레코드 타입(aai/aao/lsi) 및 Readback 동기화 로직의 어댑터 계층 구현**입니다.
