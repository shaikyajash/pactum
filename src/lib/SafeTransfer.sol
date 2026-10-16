// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

/// @notice Minimal safe ERC20 transfers. Tolerates tokens that return nothing
///         (e.g. USDT) and reverts on tokens that return `false`.
library SafeTransfer {
    error TokenTransferFailed();
    error NativeTransferFailed();

    function safeTransfer(address token, address to, uint256 amount) internal {
        _call(token, abi.encodeWithSelector(0xa9059cbb, to, amount));
    }

    function safeTransferFrom(address token, address from, address to, uint256 amount) internal {
        _call(token, abi.encodeWithSelector(0x23b872dd, from, to, amount));
    }

    function balanceOf(address token, address account) internal view returns (uint256) {
        if (token.code.length == 0) revert TokenTransferFailed();
        (bool ok, bytes memory ret) = token.staticcall(abi.encodeWithSelector(0x70a08231, account));
        if (!ok || ret.length < 32) revert TokenTransferFailed();
        return abi.decode(ret, (uint256));
    }

    function sendNative(address to, uint256 amount) internal {
        (bool ok,) = to.call{value: amount}("");
        if (!ok) revert NativeTransferFailed();
    }

    function _call(address token, bytes memory data) private {
        if (token.code.length == 0) revert TokenTransferFailed();
        (bool ok, bytes memory ret) = token.call(data);
        if (!ok || (ret.length != 0 && !abi.decode(ret, (bool)))) revert TokenTransferFailed();
    }
}
