//! On-chain bindings. Hand-written to match the Solidity interfaces so the
//! crate builds without a prior `forge build`.

// generated call builders mirror the Solidity signatures, e.g. fillWithPermit
#![allow(clippy::too_many_arguments)]

use alloy::sol;
use alloy::sol_types::SolValue;

sol! {
    #[sol(rpc)]
    contract PactumIntents {
        struct Order {
            address maker;
            address receiver;
            address srcToken;
            uint256 srcAmount;
            uint256 dstChainId;
            address dstToken;
            uint256 startDstAmount;
            uint256 endDstAmount;
            uint64 auctionStart;
            uint64 auctionEnd;
            uint64 deadline;
            uint64 srcLock;
            uint16 parts;
            bytes32 secretsRoot;
            uint256 nonce;
            uint256 minDeposit;
        }

        struct Permit {
            uint256 value;
            uint256 deadline;
            uint8 v;
            bytes32 r;
            bytes32 s;
        }

        event Initiated(
            bytes32 indexed swapId,
            address indexed initiator,
            address indexed redeemer,
            address token,
            uint256 amount,
            bytes32 secretHash,
            uint64 expiry
        );
        event Redeemed(bytes32 indexed swapId, bytes32 indexed secretHash, bytes secret);
        event Refunded(bytes32 indexed swapId);
        event DepositCredited(bytes32 indexed swapId, address indexed to, uint256 amount);

        function deposits(bytes32 swapId) external view returns (uint256);
        function depositCredit(address account) external view returns (uint256);
        function KEEPER_TIP_BPS() external view returns (uint256);
        function withdrawDeposits() external;
        event OrderFilled(
            bytes32 indexed orderHash,
            bytes32 indexed swapId,
            address indexed resolver,
            uint256 fillAmount,
            uint256 secretIndex,
            uint256 owedDstAmount
        );

        function swaps(bytes32 swapId) external view returns (
            address initiator,
            uint64 expiry,
            bool settled,
            address redeemer,
            address token,
            uint256 amount,
            bytes32 secretHash
        );
        function fills(bytes32 orderHash) external view returns (
            uint256 filled,
            uint32 nextIndex,
            bool cancelled
        );

        function initiate(
            address redeemer,
            address token,
            uint256 amount,
            bytes32 secretHash,
            uint64 expiry,
            bytes32 salt
        ) external payable returns (bytes32 swapId);
        function redeem(bytes32 swapId, bytes calldata secret) external;
        function refund(bytes32 swapId) external;
        function fill(
            Order calldata order,
            bytes calldata signature,
            uint256 fillAmount,
            uint256 secretIndex,
            bytes32 secretHash,
            bytes32[] calldata proof
        ) external payable returns (bytes32 swapId);
        function fillWithPermit(
            Order calldata order,
            bytes calldata signature,
            uint256 fillAmount,
            uint256 secretIndex,
            bytes32 secretHash,
            bytes32[] calldata proof,
            Permit calldata permit
        ) external payable returns (bytes32 swapId);
    }

    #[sol(rpc)]
    interface IERC20 {
        function approve(address spender, uint256 amount) external returns (bool);
        function allowance(address owner, address spender) external view returns (uint256);
        function balanceOf(address owner) external view returns (uint256);
    }

    #[sol(rpc)]
    interface IERC20Permit {
        function name() external view returns (string);
        function nonces(address owner) external view returns (uint256);
    }
}

pub fn permit_binding(p: &pactum_core::permit::SignedPermit) -> PactumIntents::Permit {
    PactumIntents::Permit { value: p.value, deadline: p.deadline, v: p.v, r: p.r, s: p.s }
}

/// The core and binding `Order` types are generated separately but share an ABI.
pub fn to_binding(o: &pactum_core::Order) -> PactumIntents::Order {
    PactumIntents::Order::abi_decode(&o.abi_encode()).expect("identical ABI layout")
}
