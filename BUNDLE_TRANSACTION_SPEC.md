# Bundle Transaction 타입 추가 작업 명세서

## 📋 개요

Signet L2의 atomic cross-chain swap을 지원하기 위해 OIF Solver에 Bundle Transaction 기능을 추가합니다. 이는 여러 트랜잭션을 하나의 원자적 단위로 처리할 수 있게 해주는 핵심 기능입니다.

---

## 🎯 목표

1. **Bundle Transaction 타입 정의**: 다중 트랜잭션을 하나의 번들로 관리
2. **Atomic Execution**: Host Chain과 Rollup에서 동시 실행 보장  
3. **Transaction Ordering**: Fill → Initiate 순서 강제
4. **Bundle Status Tracking**: 번들 실행 상태 모니터링

---

## 📁 파일별 수정 작업

### 1. **`crates/solver-types/src/delivery.rs`** - Bundle 타입 정의

#### 추가할 타입들

```rust
/// Bundle identifier for tracking atomic transaction groups
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BundleId(pub String);

impl BundleId {
    pub fn new() -> Self {
        BundleId(uuid::Uuid::new_v4().to_string())
    }
}

/// Bundle execution status
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BundleStatus {
    /// Bundle created but not yet submitted
    Created,
    /// Bundle submitted to cache/builder
    Submitted,
    /// Bundle accepted by block builder
    Accepted,
    /// Bundle executed successfully on host chain
    ExecutedOnHost,
    /// Bundle executed successfully on rollup
    ExecutedOnRollup,
    /// Bundle executed successfully on both chains (atomic success)
    ExecutedAtomically,
    /// Bundle failed on host chain
    FailedOnHost(String),
    /// Bundle failed on rollup
    FailedOnRollup(String),
    /// Bundle failed atomically (either chain failed)
    FailedAtomically(String),
    /// Bundle timed out
    TimedOut,
}

/// Transaction execution mode for bundle support
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionMode {
    /// Single transaction execution (existing mode)
    Single,
    /// Bundle execution for atomic cross-chain operations
    Bundle,
}

/// Bundle transaction group for atomic cross-chain execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionBundle {
    /// Unique bundle identifier
    pub bundle_id: BundleId,
    /// List of transactions in the bundle
    pub transactions: Vec<Transaction>,
    /// Execution order indices (maps to transactions vec)
    pub execution_order: Vec<usize>,
    /// Whether atomic execution is required across all chains
    pub atomic_required: bool,
    /// Target chains involved in this bundle
    pub target_chains: Vec<u64>,
    /// Bundle execution mode
    pub execution_mode: ExecutionMode,
    /// Maximum time to wait for bundle execution (seconds)
    pub timeout_seconds: Option<u64>,
    /// Metadata for bundle tracking
    pub metadata: BundleMetadata,
}

/// Bundle metadata for tracking and debugging
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleMetadata {
    /// Order ID associated with this bundle
    pub order_id: String,
    /// Bundle type description
    pub bundle_type: BundleType,
    /// Creation timestamp
    pub created_at: u64,
    /// User who initiated the bundle
    pub initiator: Address,
    /// Filler who created the bundle
    pub filler: Address,
}

/// Type of bundle for different use cases
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BundleType {
    /// Signet atomic swap bundle (Fill + Initiate)
    SignetAtomicSwap,
    /// Generic cross-chain bundle
    CrossChain,
    /// Multi-step settlement bundle
    Settlement,
}

/// Bundle execution result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleExecutionResult {
    /// Bundle identifier
    pub bundle_id: BundleId,
    /// Final execution status
    pub status: BundleStatus,
    /// Transaction hashes for each executed transaction
    pub transaction_hashes: Vec<Option<TransactionHash>>,
    /// Chain-specific results
    pub chain_results: std::collections::HashMap<u64, ChainExecutionResult>,
    /// Total execution time in milliseconds
    pub execution_time_ms: u64,
    /// Error message if failed
    pub error_message: Option<String>,
}

/// Chain-specific execution result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainExecutionResult {
    /// Chain ID
    pub chain_id: u64,
    /// Execution success on this chain
    pub success: bool,
    /// Transaction receipts for this chain
    pub receipts: Vec<TransactionReceipt>,
    /// Block number where transactions were included
    pub block_number: Option<u64>,
    /// Error message if failed on this chain
    pub error: Option<String>,
}
```

#### 기존 Transaction 확장

```rust
impl Transaction {
    /// Create a new transaction with bundle support
    pub fn with_bundle_context(mut self, bundle_id: BundleId, position: usize) -> Self {
        // Add bundle context to transaction metadata
        // Implementation details...
        self
    }
    
    /// Check if transaction is part of a bundle
    pub fn is_bundled(&self) -> bool {
        // Check if transaction has bundle context
        false // Placeholder
    }
}
```

### 2. **`crates/solver-types/src/events.rs`** - Bundle 이벤트 추가

#### DeliveryEvent 확장

```rust
/// Events related to transaction delivery operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DeliveryEvent {
    // ... 기존 이벤트들 ...
    
    /// Bundle has been created and is ready for submission
    BundleCreated {
        bundle_id: BundleId,
        order_id: String,
        transaction_count: usize,
        target_chains: Vec<u64>,
    },
    
    /// Bundle has been submitted to builder/cache
    BundleSubmitted {
        bundle_id: BundleId,
        order_id: String,
        submission_target: String, // "cache" or "builder"
    },
    
    /// Bundle execution started on a specific chain
    BundleExecutionStarted {
        bundle_id: BundleId,
        chain_id: u64,
        transaction_hashes: Vec<TransactionHash>,
    },
    
    /// Bundle confirmed on one chain (partial success)
    BundleConfirmedOnChain {
        bundle_id: BundleId,
        order_id: String,
        chain_id: u64,
        receipts: Vec<TransactionReceipt>,
    },
    
    /// Bundle atomically confirmed on all target chains
    BundleAtomicallyConfirmed {
        bundle_id: BundleId,
        order_id: String,
        result: BundleExecutionResult,
    },
    
    /// Bundle execution failed
    BundleFailed {
        bundle_id: BundleId,
        order_id: String,
        error: String,
        partial_results: Vec<ChainExecutionResult>,
    },
    
    /// Bundle timed out
    BundleTimedOut {
        bundle_id: BundleId,
        order_id: String,
        timeout_seconds: u64,
    },
}
```

### 3. **`crates/solver-types/src/order.rs`** - 주문 타입 확장

#### ExecutionParams 확장

```rust
/// Parameters for order execution, including bundle support
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionParams {
    // ... 기존 필드들 ...
    
    /// Execution mode (single or bundle)
    pub execution_mode: ExecutionMode,
    
    /// Bundle configuration if using bundle mode
    pub bundle_config: Option<BundleConfig>,
}

/// Configuration for bundle execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleConfig {
    /// Bundle type
    pub bundle_type: BundleType,
    /// Whether atomic execution is required
    pub atomic_required: bool,
    /// Maximum execution timeout
    pub timeout_seconds: u64,
    /// Transaction ordering requirements
    pub ordering_constraints: Vec<OrderingConstraint>,
}

/// Transaction ordering constraint for bundles
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderingConstraint {
    /// Transaction that must execute first
    pub before: TransactionType,
    /// Transaction that must execute after
    pub after: TransactionType,
    /// Whether this constraint is strict (bundle fails if violated)
    pub strict: bool,
}
```

### 4. **`crates/solver-types/src/lib.rs`** - Re-export 추가

```rust
// Bundle-related types
pub use delivery::{
    BundleId, BundleStatus, TransactionBundle, BundleMetadata, BundleType,
    BundleExecutionResult, ChainExecutionResult, ExecutionMode
};
pub use order::{BundleConfig, OrderingConstraint};
```

---

## 🔧 구현 단계

### Phase 1: 기본 타입 정의 (1-2일)

1. **Bundle 관련 타입 추가**
   ```bash
   # solver-types/src/delivery.rs 수정
   - BundleId, BundleStatus, TransactionBundle 구조체 추가
   - ExecutionMode enum 추가
   - BundleMetadata 및 관련 타입들 추가
   ```

2. **이벤트 시스템 확장**
   ```bash
   # solver-types/src/events.rs 수정  
   - DeliveryEvent에 Bundle 관련 이벤트 추가
   - 기존 이벤트와 호환성 유지
   ```

### Phase 2: Order 시스템 통합 (2-3일)

3. **ExecutionParams 확장**
   ```bash
   # solver-types/src/order.rs 수정
   - BundleConfig, OrderingConstraint 추가
   - 기존 ExecutionParams에 bundle 지원 추가
   ```

4. **Type 재수출**
   ```bash
   # solver-types/src/lib.rs 수정
   - 새로운 Bundle 타입들 re-export
   ```

### Phase 3: 검증 및 테스트 (1-2일)

5. **단위 테스트 작성**
   ```bash
   # solver-types/src/delivery.rs에 테스트 추가
   - Bundle 생성 테스트
   - 직렬화/역직렬화 테스트
   - Bundle 상태 전환 테스트
   ```

6. **통합 테스트**
   ```bash
   # tests/ 디렉토리에 bundle 테스트 추가
   - Bundle 라이프사이클 테스트
   - 이벤트 발생 테스트
   ```

---

## 📋 체크리스트

### 타입 정의
- [ ] `BundleId` 구조체 구현
- [ ] `BundleStatus` enum 정의  
- [ ] `TransactionBundle` 구조체 구현
- [ ] `BundleMetadata` 및 관련 타입들 정의
- [ ] `ExecutionMode` enum 추가

### 이벤트 시스템
- [ ] `DeliveryEvent`에 Bundle 이벤트들 추가
- [ ] Bundle 라이프사이클 이벤트 정의
- [ ] 기존 이벤트와 호환성 검증

### Order 시스템 통합  
- [ ] `ExecutionParams` Bundle 지원 추가
- [ ] `BundleConfig` 구현
- [ ] `OrderingConstraint` 정의

### 테스트 및 검증
- [ ] Bundle 타입 단위 테스트
- [ ] 직렬화/역직렬화 테스트
- [ ] Bundle 상태 전환 로직 테스트
- [ ] 통합 테스트 시나리오

### 문서화
- [ ] Bundle 타입 문서 작성
- [ ] 사용 예제 추가
- [ ] API 문서 업데이트

---

## 🚀 예상 결과물

### 새로운 기능
1. **Bundle Transaction 지원**: 다중 트랜잭션을 원자적 단위로 처리
2. **Atomic Cross-chain**: 여러 체인 간 원자적 실행 보장
3. **Transaction Ordering**: Fill → Initiate 순서 강제 가능
4. **Bundle Monitoring**: 번들 상태 실시간 추적

### 기존 기능과의 호환성
- 기존 단일 트랜잭션 처리 방식 100% 유지
- 점진적 마이그레이션 지원
- 이벤트 시스템 하위 호환성 보장

### 확장성
- 다른 L2/크로스체인 프로토콜 지원 기반 마련
- Bundle 기반 복잡한 DeFi 전략 지원 가능
- 향후 MEV 보호 및 최적화 기능 추가 기반

---

## ⚠️ 주의사항

### 성능 고려사항
- Bundle 생성/관리에 따른 메모리 오버헤드 최소화
- 대용량 Bundle 처리를 위한 스트리밍 지원 고려
- 동시 Bundle 처리 시 리소스 경합 방지

### 에러 처리
- Bundle 내 일부 트랜잭션 실패 시 처리 로직
- 부분 실패 상황에서의 롤백 전략
- 타임아웃 상황 처리

### 보안 고려사항
- Bundle 구성 시 트랜잭션 간 의존성 검증
- 악의적 Bundle 구성 방지
- 권한 있는 사용자만 Bundle 생성 가능하도록 제한

이 작업 명세에 따라 구현하면 OIF Solver가 Signet L2의 Bundle 기반 atomic swap을 완전히 지원할 수 있게 됩니다. 🎯