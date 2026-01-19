# Testing Guide for atomic_websocket

## 테스트 구조

```
atomic_websocket/
├── src/
│   └── helpers/
│       ├── client_sender.rs    # Unit tests (ClientSenders)
│       ├── server_sender.rs    # Unit tests (ServerSender)
│       ├── common.rs           # Unit tests (DB functions)
│       └── ...
├── tests/                      # Integration tests
│   ├── common/mod.rs           # 공용 테스트 유틸리티
│   ├── client_server_test.rs   # 서버-클라이언트 연결 테스트
│   ├── message_test.rs         # 메시지 인코딩/디코딩 테스트
│   ├── reconnection_test.rs    # 연결/재연결 테스트
│   └── feature_test.rs         # Feature별 테스트
├── test_server/                # E2E 테스트용 서버
└── test_client/                # E2E 테스트용 클라이언트
```

## 테스트 실행 방법

### 1. 전체 테스트 실행

```bash
# 기본 features (native-db, bebop)
cargo test

# 모든 features 없이
cargo test --no-default-features

# 특정 feature만
cargo test --no-default-features --features "bebop"
```

### 2. 특정 테스트만 실행

```bash
# 특정 테스트 파일
cargo test --test client_server_test

# 특정 테스트 함수
cargo test test_server_starts_and_listens

# 특정 모듈의 테스트
cargo test --lib client_sender
```

### 3. 테스트 출력 보기

```bash
# 모든 출력 표시
cargo test -- --nocapture

# 실패한 테스트만 출력
cargo test -- --show-output
```

## E2E 테스트 (test_server + test_client)

실제 네트워크 연결을 통한 전체 기능 검증:

### 실행 방법

```bash
# 터미널 1: 서버 실행
cd test_server && cargo run

# 터미널 2: 클라이언트 실행
cd test_client && cargo run
```

### 검증 항목

- 네트워크 스캔으로 서버 발견
- WebSocket 연결 수립
- bebop 직렬화/역직렬화
- native-db 저장소 동작
- 양방향 메시지 전송
- 연결 상태 변경 감지

### 로그 확인

```bash
# 서버 로그
tail -f test_server/log/debug.log

# 클라이언트 로그
tail -f test_client/log/debug.log
```

## 테스트 종류

### Unit Tests (src/ 내부)

각 모듈의 핵심 기능을 격리하여 테스트:

| 파일 | 테스트 내용 |
|------|-------------|
| `client_sender.rs` | ClientSenders CRUD, 브로드캐스트, 타임아웃 감지 |
| `server_sender.rs` | ServerSender 상태 관리, 메시지 전송 |
| `common.rs` | DB 함수 (get_setting_by_key, set_setting) |

### Integration Tests (tests/)

| 파일 | 테스트 내용 |
|------|-------------|
| `client_server_test.rs` | 실제 WebSocket 서버 시작, 클라이언트 연결, 메시지 송수신 |
| `message_test.rs` | 메시지 인코딩/디코딩, category 변환 |
| `reconnection_test.rs` | 연결 타임아웃, 재연결 시나리오 |
| `feature_test.rs` | Feature별 조건부 컴파일 테스트 |

### E2E Tests (test_server + test_client)

| 검증 항목 | 설명 |
|-----------|------|
| 네트워크 스캔 | 로컬 네트워크에서 서버 자동 발견 |
| bebop 프로토콜 | AppStartup/AppStartupOutput 메시지 교환 |
| native-db | 설정 저장/로드 |
| 연결 유지 | 2초 간격 ping-pong |

## Feature 조합별 테스트

| Feature | 테스트 명령 |
|---------|-------------|
| 기본 (native-db + bebop) | `cargo test` |
| In-memory only | `cargo test --no-default-features` |
| bebop only | `cargo test --no-default-features --features bebop` |
| rustls | `cargo test --features rustls` |

## 테스트 작성 가이드

### Unit Test 예시

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_function_name() {
        // Arrange
        let input = create_test_input();

        // Act
        let result = function_to_test(input);

        // Assert
        assert_eq!(result, expected);
    }

    #[tokio::test]
    async fn test_async_function() {
        let result = async_function().await;
        assert!(result.is_ok());
    }
}
```

### Feature 조건부 테스트

```rust
#[cfg(not(feature = "native-db"))]
#[test]
fn test_in_memory_only() {
    // native-db feature가 비활성화된 경우에만 실행
}

#[cfg(feature = "bebop")]
#[test]
fn test_bebop_feature() {
    // bebop feature가 활성화된 경우에만 실행
}
```

## CI/CD 권장 설정

```yaml
# GitHub Actions 예시
test:
  runs-on: ubuntu-latest
  steps:
    - uses: actions/checkout@v4
    - name: Run tests (default features)
      run: cargo test
    - name: Run tests (no-default-features)
      run: cargo test --no-default-features
```
