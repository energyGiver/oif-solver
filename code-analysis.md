# OIF Solver 코드 분석 (Code-Referenced Overview)

본 문서는 시퀀스 다이어그램 관점에서 oif-solver의 End-to-end 흐름을 실제 코드(파일/함수)와 정확히 매핑하고, 각 핵심 모듈(수익성, 전략, 딜리버리, 정산, 상태/스토리지, 설정)을 코드 레벨로 요약 정리합니다.

---

## 1) End-to-end 플로우 ↔ 코드 매핑

다이어그램 단계별로 어떤 파일/함수가 실행되는지 빠르게 추적할 수 있도록 정리했습니다.

- Intent 수신 → 검증 → 전략 판단
  - 위치: `crates/solver-core/src/handlers/intent.rs`
    - Dedup/저장 슬롯 선점: `storage.exists` → `storage.store` with `StorageKey::Intents`
    - 주문 생성/검증: `OrderService::validate_and_create_order` (in `crates/solver-order/src/lib.rs`)
    - 비용 추정/수익성 검증:
      - `CostProfitService::estimate_cost_for_order`
      - `CostProfitService::validate_profitability` (임계값: `config.solver.min_profitability_pct`)
    - 실행 컨텍스트: `ContextBuilder::build_execution_context` (in `crates/solver-core/src/engine/context.rs`)
    - 전략 판단: `OrderService::should_execute` → `ExecutionDecision::{Execute|Skip|Defer}`
    - 이벤트 발행: `DiscoveryEvent::IntentValidated`, `OrderEvent::{Preparing|Skipped|Deferred}`

- 준비/실행(트랜잭션 생성·제출)
  - 위치: `crates/solver-core/src/handlers/order.rs`
    - 준비 Tx(옵션): `OrderService::generate_prepare_transaction` → `DeliveryService::deliver`
      - 상태/저장: `OrderStatus::Pending`, `StorageKey::OrderByTxHash` 역매핑 저장
      - 이벤트: `DeliveryEvent::TransactionPending { tx_type: Prepare }`
    - 실행(fill) Tx: `OrderService::generate_fill_transaction` → `DeliveryService::deliver`
      - 상태/저장: `set_transaction_hash(.., TransactionType::Fill)` + 역매핑 저장
      - 이벤트: `DeliveryEvent::TransactionPending { tx_type: Fill }`

- Tx 확정 처리/상태 전이
  - 위치: `crates/solver-core/src/handlers/transaction.rs`
    - Prepare 확정: `handle_prepare_confirmed` → `OrderStatus::Executing` → `OrderEvent::Executing`
    - Fill 확정: `handle_fill_confirmed` → `OrderStatus::Executed` → `SettlementEvent::PostFillReady`
    - PostFill 확정: `handle_post_fill_confirmed` → `OrderStatus::PostFilled` → `SettlementEvent::StartMonitoring { fill_tx_hash }`
    - PreClaim 확정: `handle_pre_claim_confirmed` → `OrderStatus::PreClaimed` → `SettlementEvent::ClaimReady`
    - Claim 확정: `handle_claim_confirmed` → `OrderStatus::Finalized` → `SettlementEvent::Completed`
    - Settlement 콜백: PostFill/PreClaim 시 `SettlementInterface::handle_transaction_confirmed`

- 정산(Post-Fill/Pre-Claim/Claim)
  - 위치: `crates/solver-core/src/handlers/settlement.rs`
    - PostFill 준비: `SettlementHandler::handle_post_fill_ready`
      - Fill receipt: `DeliveryService::get_receipt`
      - Tx 생성: `SettlementService::generate_post_fill_transaction` → 필요 시 `DeliveryService::deliver`
    - 정산 모니터링 시작: `SettlementEvent::StartMonitoring` (또는 PostFill 생략 시 바로 시작)
    - PreClaim 준비: `SettlementHandler::handle_pre_claim_ready`
      - Tx 생성: `SettlementService::generate_pre_claim_transaction` → 필요 시 `DeliveryService::deliver`
    - Claim 배치: `SettlementHandler::process_claim_batch`
      - 증빙: `order.fill_proof` → `OrderService::generate_claim_transaction` → `DeliveryService::deliver`

- 딜리버리/체인데이터
  - 위치: `crates/solver-delivery/src/lib.rs`
    - 제출: `DeliveryService::deliver`
    - 확인/영수증: `confirm_with_default`, `get_receipt`
    - 가스/밸런스/논스: `get_gas_price`, `get_balance`, `get_nonce`, `estimate_gas`

---

## 2) 모듈별 심화 요약 (코드 기준)

### A. 디스커버리(Discovery)
- 위치: `crates/solver-discovery/`
- 역할: 새로운 인텐트를 온체인/오프체인에서 발견해 `Intent`로 표준화하고 퍼블리시
- 개요:
  - 온체인(EIP-7683): 체인 이벤트(예: Open)를 구독/폴링 → 표준화된 Intent 생성
  - 오프체인(EIP-7683): HTTP API로 주문 접수 → 주문 유효성/ID 계산 → Intent 생성
  - 이후 이벤트 버스를 통해 `IntentHandler::handle`로 전달되어 검증/실행 판단 흐름 진입
- 관련 설정: `config/*`의 `[discovery.implementations.*]` 섹션 (네트워크/폴링간격/API 등)

### B. 수익성(Profitability)
- 위치: `crates/solver-core/src/engine/cost_profit.rs`
- 사용 흐름: `IntentHandler::handle` → 원가 추정 → 수익성 검증
- 핵심:
  - 비용 추정: `estimate_cost_for_order(order, config)` → 가스 유닛/가격 추정(`DeliveryService::estimate_gas`, `get_gas_price`) + `PricingService`로 USD 환산 → `CostEstimate`
  - 마진 계산: `calculate_profit_margin(order, &cost_estimate)`
    - 입력USD − 출력USD − 운영비USD = Profit; Margin = Profit / 입력USD × 100
  - 임계값 검증: `validate_profitability(order, &cost_estimate, config.solver.min_profitability_pct)`

### C. 실행 전략(Execution Strategy)
- 인터페이스: `ExecutionStrategy` in `crates/solver-order/src/lib.rs`
- 기본 구현: `crates/solver-order/src/implementations/strategies/simple.rs`
- 핵심:
  - `should_execute(order, context)`
    - 컨텍스트 최대 gas_price > `max_gas_price_gwei` → Defer(60s)
    - 출력 토큰별 solver 잔고 부족/정보없음 → Skip 사유 반환
    - 조건 충족 → `ExecutionParams { gas_price, priority_fee }`와 함께 Execute
  - 설정 검증: `SimpleStrategySchema` (TOML의 `max_gas_price_gwei` 검증)

### D. Delivery (제출/조회/체인 데이터)
- 인터페이스: `DeliveryInterface` (체인별 구현; EVM Alloy)
- 서비스: `DeliveryService` (체인 ID 라우팅)
- 주요 메서드: `deliver`, `confirm(_with_default)`, `get_receipt`, `get_chain_data`, `estimate_gas`, `get_balance`, `get_nonce`, `get_allowance`
- 사용지점: `OrderHandler`/`SettlementHandler`에서 제출, `TransactionHandler`/모니터에서 확인/조회

### E. Settlement (정산 수명주기)
- 인터페이스/서비스: `crates/solver-settlement/src/lib.rs`
  - `SettlementInterface::{get_attestation, can_claim, generate_post_fill_transaction, generate_pre_claim_transaction, handle_transaction_confirmed}`
  - `SettlementService::{get_attestation, can_claim, generate_post_fill_transaction, generate_pre_claim_transaction, find_settlement_for_order}`
- 핸들러: `crates/solver-core/src/handlers/settlement.rs`
- 구현 예시: `implementations/{direct,hyperlane}.rs` (직접 검증/하이퍼레인 메시지 추적 등)

### F. 상태/스토리지(State/Storage)
- 저장 키: `solver_types::StorageKey::{Intents, Orders, OrderByTxHash}`
- FSM: `OrderStateMachine::{store_order, set_transaction_hash, transition_order_status, update_order_with}`
- Tx ↔ Order 역매핑: `StorageKey::OrderByTxHash` (hex 인코딩된 tx 해시를 키로 사용)
- 상태 전이 소스: `TransactionHandler::{handle_*_confirmed, handle_failed}`, `OrderHandler::{handle_preparation, handle_execution}`, `SettlementHandler::{handle_post_fill_ready, handle_pre_claim_ready, process_claim_batch}`

### G. 설정(Config)
- 위치: `config/*.toml`, `config/{demo,testnet}/*.toml`
- 주요 필드:
  - `[solver]` → `min_profitability_pct`, `monitoring_timeout_minutes`
  - `[delivery.implementations.*]` → 체인별 전달 구현/확인 수/폴링 간격
  - `[discovery.implementations.*]` → 온/오프체인 디스커버리 설정
  - `[settlement.implementations.*]` → 오라클 라우트/선택전략/폴링 간격
- 코드 연결: 위 임계값/폴링/확인 수는 각각 `CostProfitService`, `SettlementService::new`, `DeliveryService::new` 경유로 사용

---

## 3) 핵심 트레잇/서비스 레퍼런스(요약)

- Order
  - 인터페이스: `OrderInterface` (in `crates/solver-order/src/lib.rs`)
    - `validate_and_create_order`, `validate_order`
    - `generate_prepare_transaction` (옵션), `generate_fill_transaction`, `generate_claim_transaction`
  - 서비스: `OrderService`
    - `should_execute`, `generate_*`, `validate_*`

- Strategy
  - 인터페이스: `ExecutionStrategy::should_execute`
  - 기본 구현: `implementations/strategies/simple.rs`

- Delivery
  - 인터페이스: `DeliveryInterface::{submit, wait_for_confirmation, get_receipt, get_gas_price, get_balance, get_allowance, get_nonce, get_block_number, estimate_gas, eth_call}`
  - 서비스: `DeliveryService::{deliver, confirm(_with_default), get_receipt, get_chain_data, ...}`

- Settlement
  - 인터페이스: `SettlementInterface::{get_attestation, can_claim, generate_post_fill_transaction, generate_pre_claim_transaction, handle_transaction_confirmed}`
  - 서비스: `SettlementService::{get_attestation, can_claim, generate_post_fill_transaction, generate_pre_claim_transaction, find_settlement_for_order}`

---

## 4) 빠른 파일 경로 인덱스

- Intent/Order/Tx/Settlement 핸들러
  - `crates/solver-core/src/handlers/{intent,order,transaction,settlement}.rs`
- 엔진 유틸리티
  - 컨텍스트: `crates/solver-core/src/engine/context.rs`
  - 수익성: `crates/solver-core/src/engine/cost_profit.rs`
- 오더/전략
  - `crates/solver-order/src/lib.rs`, `crates/solver-order/src/implementations/strategies/simple.rs`
- 정산
  - `crates/solver-settlement/src/lib.rs`, `crates/solver-settlement/src/implementations/{direct,hyperlane}.rs`
- 딜리버리
  - `crates/solver-delivery/src/lib.rs`
- 설정 샘플
  - `config/*.toml`, `config/{demo,testnet}/*.toml`

---

## 5) 다음 단계 제안

- 본 파일을 기준으로 각 단계별 세부 문서를 분리(Discovery/Validation, Execution, Settlement)해 심화 예시 및 스니펫을 추가할 수 있습니다.
- 특정 구현(Hyperlane/Direct)의 로그 파싱/메시지 추적 경로에 대한 코드 스니펫을 포함한 “Troubleshooting 가이드”를 원하시면 이어서 작성하겠습니다.

---

## 6) 흐름별 코드 상세 보충

### 6.1 Intent discovery & processing (핵심 구조체/메서드)

핵심 데이터 구조체
- `solver_types::Intent` — id, source, standard, order_bytes, data, lock_type, quote_id
- `StandardOrder` — `solver_types::standards::eip7683::interfaces::StandardOrder` (EIP-7683 ABI 디코드 결과)
- `Order` — `solver_types::order::Order` (id, standard, status, data=Eip7683OrderData JSON, solver_address, input_chains, output_chains)
- `Eip7683OrderData` — `solver_types::standards::eip7683::Eip7683OrderData` (order_id, lock_type, raw_order_data, inputs/outputs, deadlines)
- `ChainSettlerInfo` — `solver_types::order::ChainSettlerInfo` (chain_id, settler_address)
- `NetworksConfig` — `solver_types::NetworksConfig` (체인별 settler 주소/RPC 등)
- `LockType` — `solver_types::standards::eip7683::LockType` (Permit2Escrow, Eip3009Escrow, ResourceLock[Compact])

검증 (validate intent)
- Entry: `OrderService::validate_order(standard, order_bytes)` in `crates/solver-order/src/lib.rs`
- EIP-7683: `_7683.rs::validate_order(order_bytes)`
  - ABI 디코드: `StandardOrder::abi_decode(order_bytes, true)` 실패 시 ValidationFailed
  - 시간 제약: `expires`, `fillDeadline` > `current_timestamp()`
  - 오라클 라우트: `oracle_routes.supported_routes`로 origin→destination 지원/호환 oracle 확인

Intent → Order 변환
- Entry: `OrderService::validate_and_create_order(standard, order_bytes, intent_data, lock_type, order_id_callback, solver_address)`
- 표준 구현: `Eip7683OrderImpl::validate_and_create_order` (`_7683.rs`)
1) `validate_order(order_bytes)` 재사용
2) `lock_type.parse::<LockType>()`
3) 원본 체인 input settler 조회: `get_settler_address(origin_chain_id, lock_type)`
4) 주문 ID용 calldata: `build_order_id_call(order_bytes, lock_type)` (Compact=IInputSettlerCompact, Escrow=IInputSettlerEscrow)
5) 주문 ID 계산: `tx_data = [settler_address || calldata]` → `order_id_callback(chain_id, tx_data)` (32바이트 길이 검증)
6) 체인/settler 세팅: `NetworksConfig`로 `input_chains`/`output_chains` 채움
7) `Eip7683OrderData` 구성: `try_from(intent_data)` 성공 시 재사용, 실패 시 `from(StandardOrder)`; `order_id`/`lock_type`/`raw_order_data` 설정
8) 최종 `Order { standard: "eip7683", status: Pending, data, solver_address, input_chains, output_chains, ... }`

상위 오케스트레이션
- 위치: `crates/solver-core/src/handlers/intent.rs::IntentHandler::handle`
1) 중복 방지: `storage.exists` → 없으면 `store(StorageKey::Intents, ...)`
2) on-chain 발견 시 intent.id를 반환하는 `order_id_callback` 구성
3) `OrderService::validate_and_create_order(..)`로 Order 생성
4) 비용추정/수익성(`CostProfitService`) → 컨텍스트(`ContextBuilder`) → 전략 판단(`should_execute`) → 이벤트 발행

### 6.2 Intent Execution

check execution strategy에서 무엇을 체크하나
- Entry: `ExecutionStrategy::should_execute(order, context)` (svc: `OrderService::should_execute`)
- 기본 구현: `crates/solver-order/src/implementations/strategies/simple.rs`
1) 가스 상한: 컨텍스트의 체인별 gas_price 최대값 > `max_gas_price_gwei` → `Defer(duration)`
2) 잔고: 요청 출력 자산별 solver 잔고 부족/미존재 → `Skip(reason)`
3) 실행: 조건 통과 시 `Execute(ExecutionParams { gas_price, priority_fee })`
- 설정: `SimpleStrategySchema`로 `max_gas_price_gwei` 등 TOML 검증

수익성 계산 → Fill 트랜잭션 생성/제출
- 비용/수익성(코어): `crates/solver-core/src/engine/cost_profit.rs`
  - `estimate_cost_for_order(order, config)` → `DeliveryService::estimate_gas`/`get_gas_price` + `PricingService` USD 환산 → `CostEstimate`
  - `validate_profitability(order, &CostEstimate, config.solver.min_profitability_pct)` → 마진 임계 검증
- Fill Tx 생성(오더): `OrderService::generate_fill_transaction(order, params)`
  - EIP-7683: `_7683.rs::generate_fill_transaction` → `IOutputSettlerSimple::fill` calldata, to=`output_settler_address`, chain=`dest`
- 제출(핸들러): `crates/solver-core/src/handlers/order.rs::handle_execution` → `DeliveryService::deliver` (tx 해시 저장/이벤트 발행)

### 6.3 Post-Fill processing

Delivery service는 어디에 제출하며, 설정은 어디에 있나
- 제출 경로: `DeliveryService::deliver` → 체인별 구현 라우팅
  - EVM: `crates/solver-delivery/src/implementations/evm/alloy.rs` → provider+wallet로 `send_transaction` → tx hash 반환, 확인/영수증/가스/잔고 유틸 제공
- 제출 대상:
  - Fill: 목적지 체인의 `output_settler_address` (EIP-7683 fill)
  - PostFill/PreClaim/Claim: 각 생성 트랜잭션의 `to`/`chain_id`에 따름
- 설정 위치:
  - TOML: `config/*.toml`의 `[delivery.implementations.*]` (네트워크 매핑, 계정/확인수/폴링)
  - 네트워크/settler: `NetworksConfig` (예: `config/*/networks.toml` 계열)에서 로드되어 Order/Settlement 생성 시 주입

PostFill이란 무엇인가 (Submit PostFill)
- 정의: Fill 후 정산 준비/증명 전달을 위해 목적지 체인에서 수행하는 선택적 후속 트랜잭션
- 생성: `SettlementService::generate_post_fill_transaction(order, fill_receipt)`
  - Direct(`crates/solver-settlement/src/implementations/direct.rs`): 필요 시 간단 Tx 또는 None
  - Hyperlane(`implementations/hyperlane.rs`): 메시지 전송/IGP 가스 결제, messageId 추적, 확정 시 `handle_transaction_confirmed`로 상태 반영
- 제출: `crates/solver-core/src/handlers/settlement.rs::handle_post_fill_ready` → `DeliveryService::deliver`
- 확정 후: `TransactionHandler::handle_post_fill_confirmed` → `OrderStatus::PostFilled` → `SettlementEvent::StartMonitoring`로 모니터링 개시

### 6.4 Settlement monitoring

Start monitoring for Claim readiness는 무엇을 하나
- Entry: `crates/solver-core/src/monitoring/settlement.rs::SettlementMonitor::monitor_claim_readiness`
1) 증빙 획득: `SettlementService::get_attestation(order, fill_tx_hash)`
   - Direct: 목적지 체인(receipt+block timestamp)에서 `FillProof { filled_timestamp, ... }` 생성
   - Hyperlane: PostFill 메시지 ID/증명 경로 관리, 필요한 경우 isProven 쿼리(오라클)로 증빙 판단
2) 증빙 저장: `OrderStateMachine::set_fill_proof`
3) 주기적 확인: `SettlementService::can_claim(order, &fill_proof)`를 poll (interval은 `SettlementService::poll_interval_seconds()`)
   - Direct: 목적지 체인에서 현재 시각 − `filled_timestamp` ≥ `dispute_period_seconds` 확인
   - Hyperlane: 오라클 증명 완료 상태(true) 확인
4) 준비 완료: FSM `OrderStatus::Settled` → `SettlementEvent::PreClaimReady` 발행

체인 관점 정리
- get_attestation: 일반적으로 목적지(output) 체인 receipt/이벤트에서 fill 사실을 추출
- can_claim: Direct 기준으로 목적지 체인의 시간 경과 확인(분쟁기간), Hyperlane은 증명 완료 여부 확인
- 이후 Pre-Claim/Claim은 입력(origin) 체인에서 진행 (EIP-7683의 최종 보상 청구는 origin 체인의 input settler 호출)

### 6.5 Pre-Claim & Claim

정의
- Pre-Claim: 최종 Claim 전에 필요한 오라클/브릿지 상호작용을 완료하는 사전 준비 트랜잭션(선택적)
- Claim: 원본(origin) 체인의 Settler에서 솔버 보상을 수령하는 최종 트랜잭션

차이
- 시점/체인: Pre-Claim은 보통 증명 체인(입력/origin 또는 특정 오라클 체인)에서 증빙을 마무리, Claim은 origin 체인의 input settler 호출
- 생성 위치:
  - Pre-Claim: `SettlementService::generate_pre_claim_transaction(order, fill_proof)` (예: Direct는 origin 체인 input oracle 상호작용 Tx 선택적 생성)
  - Claim: `OrderService::generate_claim_transaction(order, fill_proof)` (EIP-7683 `finalise`/`finaliseSelf` on origin input settler)
- 제출/확정 핸들링:
  - Pre-Claim 제출: `SettlementHandler::handle_pre_claim_ready` → `DeliveryService::deliver` → `TransactionHandler::handle_pre_claim_confirmed` → `OrderStatus::PreClaimed` → `SettlementEvent::ClaimReady`
  - Claim 제출: `SettlementHandler::process_claim_batch` → `DeliveryService::deliver` → `TransactionHandler::handle_claim_confirmed` → `OrderStatus::Finalized` → `SettlementEvent::Completed`
