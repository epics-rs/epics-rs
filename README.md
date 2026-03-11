# epics-rs

Pure Rust implementation of the [EPICS](https://epics-controls.org/) control system framework.

No C dependencies. No `libca`. No `libCom`. Just `cargo build`.

C EPICS 클라이언트(`caget`, `camonitor`, CSS 등)와 **와이어 레벨에서 100% 호환**됩니다.

## Overview

epics-rs는 C/C++ EPICS의 핵심 구성요소를 Rust로 재구현한 프로젝트입니다:

- **Channel Access 프로토콜** — 클라이언트 & 서버 (UDP 네임 리졸루션 + TCP 가상 회선)
- **IOC 런타임** — 20개 레코드 타입, .db 파일 로딩, 링크 체인, 스캔 스케줄링
- **asyn 프레임워크** — 액터 기반 비동기 포트 드라이버 모델
- **모터 레코드** — 9단계 상태 머신, 좌표 변환, 백래시 보상
- **areaDetector** — NDArray, 드라이버 베이스, 16개 플러그인
- **시퀀서** — SNL 컴파일러 + 런타임
- **calc 엔진** — 수식/문자열/배열 연산
- **autosave** — PV 저장/복원
- **msi** — 매크로 치환 & 인클루드 도구

## Workspace Structure

```
epics-rs/
├── crates/
│   ├── epics-base/       # CA 프로토콜, IOC 런타임, 20개 레코드 타입, iocsh
│   ├── epics-macros/     # #[derive(EpicsRecord)] proc macro
│   ├── asyn/             # 비동기 디바이스 I/O 프레임워크 (포트 드라이버 모델)
│   ├── motor/            # 모터 레코드 + SimMotor
│   ├── ad-core/          # areaDetector 코어 (NDArray, NDArrayPool, 드라이버 베이스)
│   ├── ad-plugins/       # 16개 NDPlugin (Stats, ROI, FFT, TIFF, JPEG, HDF5 등)
│   ├── calc/             # Calc 수식 엔진 (수치, 문자열, 배열, 수학 함수)
│   ├── seq/              # 시퀀서 런타임 (상태 머신 실행)
│   ├── snc-core/         # SNL 컴파일러 라이브러리 (lexer, parser, codegen)
│   ├── snc/              # SNL 컴파일러 CLI
│   ├── autosave/         # PV 자동 저장/복원
│   ├── busy/             # Busy 레코드
│   └── msi/              # 매크로 치환 & 인클루드 도구 (.template → .db)
└── examples/
    ├── scope-ioc/        # 디지털 오실로스코프 시뮬레이터
    ├── mini-beamline/    # 모터 5축 + 검출기를 갖춘 빔라인 시뮬레이터
    ├── sim-detector/     # areaDetector 시뮬레이션 드라이버
    └── seq-demo/         # 시퀀서 데모
```

### Crate Dependency Graph

```
epics-base-rs ◄─── epics-macros (proc macro)
    ▲
    ├── calc-rs (epics feature)
    ├── autosave-rs
    ├── busy-rs
    ├── seq
    │    └── snc-core
    ├── asyn-rs (epics feature)
    │    └── motor-rs
    └── ad-core (ioc feature)
         ├── asyn-rs
         └── ad-plugins
              └── asyn-rs

msi-rs (standalone — EPICS 의존성 없음)
```

## Architecture: C EPICS vs epics-rs

### 핵심 설계 차이점

| 측면 | C EPICS | epics-rs |
|------|---------|----------|
| **동시성 모델** | POSIX 스레드 + 뮤텍스 풀 + 이벤트 큐 | tokio async + 드라이버별 액터 (exclusive ownership) |
| **레코드 내부** | C 구조체 필드, `dbAddr` 포인터 산술 | Rust 트레이트 시스템, 온디맨드 `Snapshot` 생성 |
| **디바이스 드라이버** | C 함수 + `void*` 포인터 | Rust 트레이트 + impl 블록 (타입 안전) |
| **메타데이터 저장** | 레코드 C 구조체에 직접 저장 (flat memory) | `Snapshot`에 온디맨드 조립 (Display/Control/EnumInfo) |
| **모듈 시스템** | `.dbd` 파일 + `Makefile` | Cargo workspace + feature flags |
| **링크 해석** | `dbAddr` 포인터 오프셋 | 트레이트 메서드 + 필드명 디스패치 |
| **메모리 안전** | 수동 관리 (segfault 가능) | Safe Rust (레코드 로직에 unsafe 없음) |
| **IOC 설정** | `st.cmd` 쉘 스크립트 | Rust 빌더 API 또는 `st.cmd` 호환 파서 |
| **와이어 포맷** | CA 프로토콜 | **동일** (C 클라이언트/서버와 완전 호환) |

### 1. Actor 기반 동시성

C EPICS는 전역 공유 상태에 뮤텍스 풀을 사용합니다. epics-rs는 드라이버당 tokio 액터가 독점 소유권을 가지며, 핫 패스에 `Arc<Mutex>`가 없습니다:

```
C EPICS:                          epics-rs:
┌──────────────────┐              ┌──────────────────┐
│  Global State    │              │   PortActor      │ ← 독점 소유
│  + Mutex Pool    │              │   (tokio task)   │
│  + Event Queue   │              ├──────────────────┤
│                  │              │   PortHandle     │ ← 클론 가능 인터페이스
│  Thread 1 ──lock─┤              │   (mpsc channel) │
│  Thread 2 ──lock─┤              └──────────────────┘
│  Thread 3 ──lock─┤
└──────────────────┘
```

### 2. Snapshot 기반 메타데이터 모델

C EPICS는 레코드 구조체의 메모리에서 직접 GR/CTRL 데이터를 읽습니다. epics-rs는 `Snapshot` 타입이 값 + 알람 + 타임스탬프 + 메타데이터를 하나로 묶습니다:

```
┌──────────────────────────────────────────────────────┐
│                     Snapshot                          │
│  value: EpicsValue                                    │
│  alarm: AlarmInfo { status, severity }                │
│  timestamp: SystemTime                                │
│  display: Option<DisplayInfo>  ← EGU, PREC, HOPR/LOPR│
│  control: Option<ControlInfo>  ← DRVH/DRVL            │
│  enums:   Option<EnumInfo>     ← ZNAM/ONAM, ZRST..FFST│
└──────────────────────────────────────────────────────┘
        │
        ▼  encode_dbr(dbr_type, &snapshot)
┌──────────────────────────────────────────────────────┐
│  DBR_PLAIN (0-6)   → bare value                      │
│  DBR_STS   (7-13)  → status + severity + value       │
│  DBR_TIME  (14-20) → status + severity + stamp + val │
│  DBR_GR    (21-27) → sts + units + prec + limits + v │
│  DBR_CTRL  (28-34) → sts + units + prec + ctrl + val │
└──────────────────────────────────────────────────────┘
```

### 3. 순수 데이터 프로토콜 타입

C EPICS의 콜백 체인 대신, epics-rs는 직렬화 가능한 메시지 타입을 사용합니다:

```rust
// 트레이트 객체나 클로저 없음 — 순수 데이터
enum PortCommand {      // 23 variants
    ReadInt32 { addr, reason },
    WriteFloat64 { addr, reason, value },
    ReadOctetArray { addr, reason, max_len },
    // ...
}
enum PortReply { ... }
enum PortEvent { ... }
```

이를 통해 향후 와이어 트랜스포트(Unix 소켓, 네트워크)로의 확장이 가능하며, 테스트가 용이합니다.

### 4. 모듈 시스템: `.dbd` → Cargo

| C EPICS | epics-rs |
|---------|----------|
| `.dbd` 파일 (모듈 선언) | `Cargo.toml` `[dependencies]` |
| `Makefile` `xxx_DBD +=` | 크레이트 의존성 추가/제거 |
| `envPaths` (빌드 시 경로 생성) | `DB_DIR` const via `CARGO_MANIFEST_DIR` |
| `registrar()` / `device()` in `.dbd` | `register_device_support()` 호출 |
| `#ifdef` 조건부 포함 | Cargo `features` |

### 5. 레코드 시스템 분리

C EPICS에서는 레코드 타입마다 별도의 `.dbd`와 C 소스가 필요합니다. epics-rs는 두 레이어로 분리합니다:

- **`record.rs`** — 모든 레코드 타입이 공유하는 인프라 (`CommonFields`, `Record` 트레이트, `RecordInstance`, 링크 파싱, 필드 get/put, 알람 로직)
- **`records/*.rs`** — 레코드 타입별 파일. `#[derive(EpicsRecord)]`로 보일러플레이트 생성

새 레코드 타입 추가 시 `record.rs`를 수정할 필요 없이 `records/`에 새 파일만 추가하면 됩니다.

## Record Types

| 타입 | 설명 | 값 타입 |
|------|------|---------|
| ai | 아날로그 입력 | Double |
| ao | 아날로그 출력 | Double |
| bi | 바이너리 입력 | Enum (u16) |
| bo | 바이너리 출력 | Enum (u16) |
| longin | 정수 입력 | Long (i32) |
| longout | 정수 출력 | Long (i32) |
| mbbi | 멀티비트 바이너리 입력 | Enum (u16) |
| mbbo | 멀티비트 바이너리 출력 | Enum (u16) |
| stringin | 문자열 입력 | String |
| stringout | 문자열 출력 | String |
| waveform | 배열 데이터 | DoubleArray / LongArray / CharArray |
| calc | 수식 계산 | Double |
| calcout | 수식 + 출력 | Double |
| fanout | 포워드 링크 팬아웃 | — |
| dfanout | 데이터 팬아웃 | Double |
| seq | 시퀀스 | Double |
| sel | 셀렉트 | Double |
| compress | 원형 버퍼 / N-to-1 압축 | DoubleArray |
| histogram | 시그널 히스토그램 | LongArray |
| sub | 서브루틴 | Double |

## Quick Start

### Build

```bash
cargo build --workspace
```

### Soft IOC 실행

```bash
# 간단한 PV
softioc-rs --pv TEMP:double:25.0 --pv MSG:string:hello

# 레코드 기반
softioc-rs --record ai:SENSOR:0.0 --record bo:SWITCH:0

# .db 파일에서
softioc-rs --db my_ioc.db -m "P=TEST:,R=TEMP"
```

### CA 클라이언트 도구

```bash
caget-rs TEMP              # 읽기
caput-rs TEMP 42.0          # 쓰기
camonitor-rs TEMP           # 구독
cainfo-rs TEMP              # 메타데이터
```

C EPICS 클라이언트(`caget`, `camonitor`, CSS, PyDM 등)도 그대로 사용할 수 있습니다.

### 라이브러리로 사용

#### 선언적 IOC 빌더

```rust
use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::bi::BiRecord;

IocApplication::new()
    .record("TEMP", AoRecord::new(25.0))
    .record("INTERLOCK", BiRecord::new(0))
    .run()
    .await?;
```

#### IocApplication (st.cmd 스타일)

```rust
use epics_base_rs::server::ioc_app::IocApplication;

IocApplication::new()
    .register_device_support("myDriver", || Box::new(MyDeviceSupport::new()))
    .startup_script("ioc/st.cmd")
    .run()
    .await?;
```

st.cmd는 **C++ EPICS와 동일한 문법**을 사용합니다:

```bash
epicsEnvSet("PREFIX", "SIM1:")
myDriverConfig("SIM1", 256, 256, 50000000)
dbLoadRecords("$(MY_DRIVER)/Db/myDriver.db", "P=$(PREFIX)")
iocInit()
```

#### CA 클라이언트 라이브러리

```rust
use epics_base_rs::client::CaClient;

let client = CaClient::new().await?;
let (_type, value) = client.caget("TEMP").await?;
client.caput("TEMP", "42.0").await?;
```

## Crate Details

### epics-base-rs

CA 프로토콜 클라이언트/서버, IOC 런타임, 20개 레코드 타입, iocsh, 액세스 보안, autosave 통합.

- UDP 네임 리졸루션 + TCP 가상 회선
- 확장 CA 헤더 (>64 KB 페이로드)
- 비콘 에미터, 모니터 구독
- ACF 파일 파서 (UAG/HAG/ASG 규칙)
- pvAccess 클라이언트 (실험적)

### asyn-rs

C EPICS asyn의 Rust 포트. 액터 기반 포트 드라이버 모델:

- **PortDriver 트레이트** — `read_int32`, `write_float64`, `read_octet_array` 등
- **ParamList** — 변경 추적, 타임스탬프, 알람 전파
- **PortActor** — 드라이버 독점 소유 (tokio task)
- **PortHandle** — 클론 가능한 비동기 인터페이스
- **RuntimeClient** — 트랜스포트 추상화 (InProcessClient, 향후 UnixSocketClient)

### motor-rs

완전한 모터 레코드 구현:

- **9단계 모션 상태 머신** — Idle, MainMove, BacklashApproach, BacklashFinal, Retry, Jog, JogStopping, JogBacklash, Homing
- **좌표 변환** — User ↔ Dial ↔ Raw (스텝)
- **백래시 보상** — 접근 + 최종 이동
- **4가지 재시도 모드** — Default, Arithmetic, Geometric, InPosition
- **AxisRuntime** — 축별 tokio 액터, 폴링 루프
- **SimMotor** — 테스트용 시간 기반 선형 보간 모터

### ad-core & ad-plugins

areaDetector 프레임워크:

- **NDArray** — N차원 타입 배열 (10개 데이터 타입)
- **NDArrayPool** — 프리 리스트 버퍼 재사용
- **ADDriverBase** — 검출기 드라이버 베이스 (Single/Multiple/Continuous 모드)
- **16개 플러그인** — Stats, ROI, Process, Transform, ColorConvert, Overlay, FFT, TimeSeries, CircularBuff, Codec, Gather, Scatter, StdArrays, FileTIFF, FileJPEG, FileHDF5

### calc-rs

수식 엔진:

- **수치 연산** — infix→postfix 컴파일, 16개 입력 변수 (A-P), 수학 함수
- **문자열 연산** — 문자열 조작, 12개 문자열 변수 (AA-LL)
- **배열 연산** — 요소별 연산, 통계 (mean, sigma, min, max, median)
- **EPICS 레코드** — transform, scalcout, sseq (epics feature)

### seq & snc-core

EPICS 시퀀서:

- **런타임 (seq)** — 상태 셋 실행, pvGet/pvPut/pvMonitor, 이벤트 플래그
- **컴파일러 (snc-core)** — SNL 렉서/파서, AST, IR, 시맨틱 분석, Rust 코드 생성

### autosave-rs

PV 자동 저장/복원:

- 주기적/트리거/변경 시/수동 저장
- 원자적 파일 쓰기 (tmp → fsync → rename)
- 백업 로테이션 (`.savB`, 시퀀스 파일, 날짜 백업)
- C autosave 호환 포맷

### msi-rs

매크로 치환 & 인클루드 도구:

- `.template` → `.db` 변환
- `$(KEY)`, `${KEY}`, `$(KEY=default)`, `$$` 이스케이프
- C EPICS MSI 호환

## Examples

### scope-ioc — 디지털 오실로스코프 시뮬레이터

1 kHz 사인파 (1000포인트), 노이즈/게인/트리거 설정. asyn PortDriver 기반.

```bash
cargo run --bin scope_ioc
```

### mini-beamline — 빔라인 시뮬레이터

빔 전류 시뮬레이터, 3개 포인트 검출기, MovingDot 2D 영역 검출기, 5축 모터 레코드.

```bash
cargo run --bin mini_ioc
```

### sim-detector — areaDetector 시뮬레이션

시뮬레이션된 areaDetector 드라이버 IOC.

```bash
cargo run --bin sim_ioc --features sim-detector/ioc
```

## Binaries

### Channel Access 도구

| 바이너리 | 설명 |
|----------|------|
| `caget-rs` | PV 값 읽기 |
| `caput-rs` | PV 값 쓰기 |
| `camonitor-rs` | PV 변경 구독 |
| `cainfo-rs` | PV 메타데이터 표시 |
| `ca-repeater-rs` | CA 네임 리졸버 |

### pvAccess 도구 (실험적)

| 바이너리 | 설명 |
|----------|------|
| `pvaget-rs` | PVA 읽기 |
| `pvaput-rs` | PVA 쓰기 |
| `pvamonitor-rs` | PVA 구독 |
| `pvainfo-rs` | PVA 메타데이터 |

### IOC & 도구

| 바이너리 | 설명 |
|----------|------|
| `softioc-rs` | Soft IOC 서버 |
| `snc` | SNL 컴파일러 |
| `msi-rs` | 매크로 치환 도구 (cli feature) |

## Feature Flags

| Crate | Feature | Default | 설명 |
|-------|---------|---------|------|
| `asyn-rs` | `epics` | no | epics-base 어댑터 브릿지 활성화 |
| `calc-rs` | `numeric` | yes | 수치 수식 엔진 |
| `calc-rs` | `string` | no | 문자열 수식 |
| `calc-rs` | `array` | no | 배열 수식 |
| `calc-rs` | `math` | no | 고급 수학 함수 (미분, 피팅, 보간) |
| `calc-rs` | `epics` | no | EPICS 레코드 통합 (transform, scalcout, sseq) |
| `ad-core` | `ioc` | no | IOC 지원 (epics-base 포함) |
| `ad-plugins` | `ioc` | no | 플러그인 IOC 지원 |
| `ad-plugins` | `hdf5` | no | HDF5 파일 플러그인 (시스템 HDF5 라이브러리 필요) |
| `msi-rs` | `cli` | no | `msi-rs` CLI 바이너리 |

## Testing

```bash
# 전체 테스트 (1,290+)
cargo test --workspace

# 옵션 feature 포함
cargo test --workspace --features calc-rs/epics,asyn-rs/epics
```

테스트 범위: 프로토콜 인코딩, 와이어 포맷 골든 패킷, 스냅샷 생성, GR/CTRL 메타데이터 직렬화, 레코드 프로세싱, 링크 체인, calc 엔진, .db 파싱, 액세스 보안, autosave, iocsh, IOC 빌더, 이벤트 스케줄링, 모터 상태 머신, asyn 포트 드라이버 등.

## Requirements

- Rust 1.70+
- tokio runtime

### Optional System Dependencies

| Feature | 라이브러리 | 설치 방법 |
|---------|-----------|----------|
| `ad-plugins/hdf5` | HDF5 C library | `brew install hdf5` (macOS) / `apt install libhdf5-dev` (Debian) |

`hdf5` feature를 제외한 모든 크레이트는 순수 Rust이며 시스템 라이브러리가 필요하지 않습니다.

## License

This project is for research and development purposes.
