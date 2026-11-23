// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {DagArbExecutor} from "../src/DagArbExecutor.sol";

interface Vm {
    function envAddress(string calldata name) external returns (address);
    function startBroadcast() external;
    function stopBroadcast() external;
}

contract DeployScript {
    Vm private constant vm = Vm(address(uint160(uint256(keccak256("hevm cheat code")))));

    function run() external returns (DagArbExecutor deployed) {
        address owner = msg.sender;
        vm.startBroadcast();
        deployed = new DagArbExecutor(owner);
        vm.stopBroadcast();
    }
}
