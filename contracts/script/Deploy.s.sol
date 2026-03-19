// SPDX-License-Identifier: MIT
pragma solidity ^0.8.23;

import {Script, console2} from "forge-std/Script.sol";
import {FlashLiquidator} from "../src/FlashLiquidator.sol";

contract Deploy is Script {
    // Arbitrum One
    address constant AAVE_PROVIDER = 0xa97684ead0e402dC232d5A977953DF7ECBaB3CDb;
    address constant UNI_ROUTER    = 0xE592427A0AEce92De3Edee1F18E0157C05861564;

    function run() external {
        address cold    = vm.envAddress("COLD_WALLET");
        uint256 privKey = vm.envUint("PRIVATE_KEY");
        address deployer = vm.addr(privKey);

        require(cold != address(0), "COLD_WALLET missing");
        require(cold != deployer,   "COLD != deployer");

        console2.log("Deployer:", deployer);
        console2.log("Cold:", cold);

        vm.startBroadcast(privKey);
        FlashLiquidator liq = new FlashLiquidator(AAVE_PROVIDER, UNI_ROUTER, cold);
        vm.stopBroadcast();

        require(liq.OWNER() == deployer, "bad owner");
        console2.log("FlashLiquidator:", address(liq));
    }
}
