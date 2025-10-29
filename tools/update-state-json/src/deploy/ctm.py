from __future__ import annotations

import os

from inputs import DeploymentInputs
from utils import cast_calldata, forge_script, read_from_file


def deploy_ctm(inputs: DeploymentInputs) -> None:
    governance_contract = read_from_file(os.path.join(inputs.l1_contracts_dir, "script-out/output-deploy-l1.toml"), "governance_addr")
    bridgehub_proxy = read_from_file(os.path.join(inputs.l1_contracts_dir, "script-out/output-deploy-l1.toml"), "bridgehub_proxy_addr")

    reuse_flag = "true" if inputs.reuse_ctm_governance else "false"
    calldata = cast_calldata("runWithBridgehub(address,bool)", bridgehub_proxy, reuse_flag)
    forge_script(
        "deploy-scripts/DeployCTM.s.sol",
        f"--ffi --rpc-url={inputs.l1_rpc_url} --broadcast --private-key={inputs.deployer_key} --sig={calldata}",
        inputs,
        "Deploy CTM stack",
    )

    state_transition_proxy_addr = read_from_file(os.path.join(inputs.l1_contracts_dir, "script-out/output-deploy-l1.toml"), "state_transition_proxy_addr")

    calldata = cast_calldata("governanceAcceptOwner(address,address)", governance_contract, state_transition_proxy_addr)
    forge_script(
        "deploy-scripts/AdminFunctions.s.sol",
        f"--ffi --rpc-url={inputs.l1_rpc_url} --broadcast --private-key={inputs.governor_key} --sig={calldata}",
        inputs,
        "Accept CTM ownership",
    )

    chain_admin_addr = read_from_file(os.path.join(inputs.l1_contracts_dir, "script-out/output-deploy-l1.toml"), "chain_admin")

    calldata = cast_calldata("chainAdminAcceptAdmin(address,address)", chain_admin_addr, state_transition_proxy_addr)
    forge_script(
        "deploy-scripts/AdminFunctions.s.sol",
        f"--ffi --rpc-url={inputs.l1_rpc_url} --broadcast --private-key={inputs.governor_key} --sig={calldata}",
        inputs,
        "Accept CTM admin",
    )

    calldata = cast_calldata("registerCTM(address,address,bool)", bridgehub_proxy, state_transition_proxy_addr, "true")
    forge_script(
        "deploy-scripts/RegisterCTM.s.sol",
        f"--ffi --rpc-url={inputs.l1_rpc_url} --broadcast --private-key={inputs.governor_key} --sig={calldata}",
        inputs,
        "Register CTM on Bridgehub",
    )
