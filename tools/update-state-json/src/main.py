from __future__ import annotations
import os
import tempfile
import json

from inputs import DeploymentInputs, initial_contracts, additional_storage
from genesis.genesis import prepare_genesis, genesis_object
from contracts_repo import clone_repository, build_contracts, copy_configs
from deploy.ecosystem import deploy_ecosystem
from deploy.ctm import deploy_ctm
from deploy.chain import bootstrap_chain
from anvil import fund_wallets, start_anvil, stop_anvil
from utils import run_command


def main(contracts_dir) -> None:
    # get inputs from env if `--from-env` is provided, otherwise use defaults
    inputs = DeploymentInputs.default()
    if "--from-env" in os.sys.argv:
        inputs = DeploymentInputs.from_env()

    state_path = "./out/zkos-l1-state.json"
    anvil = start_anvil(state_path)

    try:
        fund_wallets(inputs)

        clone_repository(contracts_dir, "zkos-v0.29.9")
        build_contracts(contracts_dir, inputs)
            
        inputs.l1_contracts_dir = f"{contracts_dir}/l1-contracts"
        copy_configs(inputs)

        initial_contracts_data = initial_contracts(inputs)
        additional_storage_data = additional_storage(inputs)
        genesis_commitment = prepare_genesis(
            initial_contracts_data,
            additional_storage_data,
        )
        inputs.genesis_commitment = genesis_commitment
        print("Genesis commitment:", genesis_commitment)

        bridgehub_proxy = deploy_ecosystem(inputs)
        deploy_ctm(inputs)
        bootstrap_chain(inputs)

        genesis = genesis_object(
            genesis_commitment,
            initial_contracts_data,
            additional_storage_data,
        )
        genesis_path = "./out/genesis.json"
        with open(genesis_path, "w") as f:
            json.dump(genesis, f, indent=4)
        print(f"\nGenesis JSON written to {genesis_path}")

        stop_anvil(anvil)
        print(f"\nAnvil state dumped to {state_path}")

        run_command(
            "Copy output configs to out/",
            f"cp -r {inputs.l1_contracts_dir}/script-out ./out/",
            ".",
            inputs.base_env(),
        )
    except Exception as e: # TODO: Not the best way to handle it, but whatever for now
        stop_anvil(anvil)
        raise e

    print("\n\n===============================\n\n")
    print("All steps completed successfully.")
    print(f"L1 BridgeHub proxy address: {bridgehub_proxy}")
    print("To run the server, do the following:")
    print("cp out/genesis.json ../../genesis/")
    print("cp out/zkos-l1-state.json ../../")
    print(f"export genesis_bridgehub_address={bridgehub_proxy}")
    print(f"export genesis_chain_id={inputs.chain_id}")
    print("Then run the server as usual.")
    print("Alternatively, update the constants in the `node/bin/src/config.rs")


if __name__ == "__main__":
    contracts_dir = "./deps/era-contracts"
    main(contracts_dir)
