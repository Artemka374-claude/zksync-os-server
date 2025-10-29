from __future__ import annotations

import os

from inputs import DeploymentInputs
from utils import cast_calldata, forge_script, read_from_file, update_toml_key, derive_address_from_private_key

def prepare_config(inputs: DeploymentInputs) -> None:
    governance_addr = derive_address_from_private_key(inputs.governor_key)

    config_values = {
        "era_chain_id": 270,
        "owner_address": governance_addr,
        "is_zk_sync_os": True,
        "governance_security_council_address": governance_addr,
        "genesis_root": inputs.genesis_commitment, # In fact, batch commitment; in ZKsync OS we use batch commitment instead of genesis root for some reason
        "genesis_rollup_leaf_index": 0, # Has to be this way for ZKsync OS
        "genesis_batch_commitment": "0x0000000000000000000000000000000000000000000000000000000000000001", # Has to be this way for ZKsync OS
        "latest_protocol_version": "0x1e00000000", # TODO: I guess should be configurable
        "create2_factory_salt": "0x7abd6010af5b4f60324f91e565b283dc2de0e3d4d1d151b9a730aaca0d760249", # TODO: dunno which value should be used, copypasted from Era
        "bootloader_hash": "0x0000000000000000000000000000000000000000000000000000000000000001", # Should not be zero (enforced by contracts), but not used
        "default_aa_hash": "0x0000000000000000000000000000000000000000000000000000000000000001", # Should not be zero (enforced by contracts), but not used
        "evm_emulator_hash": "0x0000000000000000000000000000000000000000000000000000000000000001", # Should not be zero (enforced by contracts), but not used
    }

    for key, value in config_values.items():
        cfg_path = os.path.join(inputs.l1_contracts_dir, "script-config/config-deploy-l1.toml")
        update_toml_key(cfg_path, key, value)

def deploy_ecosystem(inputs: DeploymentInputs) -> None:
    prepare_config(inputs)

    forge_script(
        "deploy-scripts/DeployL1CoreContracts.s.sol",
        f"--ffi --rpc-url={inputs.l1_rpc_url} --broadcast --private-key={inputs.deployer_key}",
        inputs,
        "Deploy L1 core ecosystem contracts",
    )

    out_cfg = os.path.join(inputs.l1_contracts_dir, "script-out/output-deploy-l1.toml")
    governance_contract = read_from_file(out_cfg, "governance_addr")
    bridgehub_proxy = read_from_file(out_cfg, "bridgehub_proxy_addr")
    shared_bridge_proxy_addr = read_from_file(out_cfg, "shared_bridge_proxy_addr")
    stm_deployment_tracker_proxy_addr = read_from_file(out_cfg, "ctm_deployment_tracker_proxy_addr")
    chain_admin_addr = read_from_file(out_cfg, "chain_admin")

    calldata = cast_calldata("governanceAcceptOwner(address,address)", governance_contract, bridgehub_proxy)
    forge_script(
        "deploy-scripts/AdminFunctions.s.sol",
        f"--ffi --rpc-url={inputs.l1_rpc_url} --broadcast --private-key={inputs.governor_key} --sig={calldata}",
        inputs,
        "Accept Bridgehub ownership (governor)",
    )
    calldata = cast_calldata("chainAdminAcceptAdmin(address,address)", chain_admin_addr, bridgehub_proxy)
    forge_script(
        "deploy-scripts/AdminFunctions.s.sol",
        f"--ffi --rpc-url={inputs.l1_rpc_url} --broadcast --private-key={inputs.governor_key} --sig={calldata}",
        inputs,
        "Accept Bridgehub admin (chain admin)",
    )

    calldata = cast_calldata("governanceAcceptOwner(address,address)", governance_contract, shared_bridge_proxy_addr)
    forge_script(
        "deploy-scripts/AdminFunctions.s.sol",
        f"--ffi --rpc-url={inputs.l1_rpc_url} --broadcast --private-key={inputs.governor_key} --sig={calldata}",
        inputs,
        "Accept L1 asset router ownership",
    )

    calldata = cast_calldata("governanceAcceptOwner(address,address)", governance_contract, stm_deployment_tracker_proxy_addr)
    forge_script(
        "deploy-scripts/AdminFunctions.s.sol",
        f"--ffi --rpc-url={inputs.l1_rpc_url} --broadcast --private-key={inputs.governor_key} --sig={calldata}",
        inputs,
        "Accept STM deployment tracker ownership",
    )
