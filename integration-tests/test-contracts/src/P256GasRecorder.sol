pragma solidity ^0.8.24;

/// @dev Calls the P256 precompile and records the remaining gas after the call.
contract P256GasRecorder {
    bytes constant P256_INPUT =
        hex"08dbe02400dac0452dfc74510137fe54393a285a3447226b96c768b5f282a9ca3b7877e9fa59378583dcc74f6debb479edbbef507aed9fc92fc522097a04ad41184d212e63ca5b0c71656bf7d4ded70734359af2a31043ebe62f5488c7efc281460a075bf70a8e58142597e76f83fcd48b8da67e9925ccf331309d0a66fd04708be96396844d445d14c5c64dc405ca38931f2cf72a70b12475e7b90364169608";
    address constant P256_PRECOMPILE = address(0x0100);

    uint256 public lastGasAfterCall;
    bool public lastSuccess;

    function callP256() external returns (bool success, uint256 gasAfter) {
        bytes memory input = P256_INPUT;
        address precompile = P256_PRECOMPILE;

        assembly {
            success := staticcall(gas(), precompile, add(input, 0x20), mload(input), 0, 0)
        }

        gasAfter = gasleft();
        lastSuccess = success;
        lastGasAfterCall = gasAfter;
    }
}
