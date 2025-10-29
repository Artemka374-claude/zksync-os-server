from __future__ import annotations

from inputs import DeploymentInputs
from utils import run_command

def fund_wallets(inputs: DeploymentInputs) -> None:
    # TODO: should take arguments from inputs
    run_command(
        "Fund commit wallet",
        "cast send 0x4bD3B5134Bf77398dE26BaD475740C1dbF1a853d --value 1000000000000000000000 --private-key 0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6",
        inputs.l1_contracts_dir,
        inputs.base_env(),
    )
    run_command(
        "Fund prove wallet",
        "cast send 0x69474D7f42b6d881BA4099431CF799B22e53D2C7 --value 1000000000000000000000 --private-key 0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6",
        inputs.l1_contracts_dir,
        inputs.base_env(),
    )
    run_command(
        "Fund execute wallet",
        "cast send 0x883FD4817fC1f4060F490375373e6bb5CC8b9e2A --value 1000000000000000000000 --private-key 0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6",
        inputs.l1_contracts_dir,
        inputs.base_env(),
    )
