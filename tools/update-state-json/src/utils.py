from __future__ import annotations

import shlex
import subprocess
from typing import Optional

from inputs import DeploymentInputs

def run_command(description: str, cmd: str, cwd: str, env: dict) -> None:
    print(f"\n=== {description} ===")
    print(f"$ {cmd}")
    args = shlex.split(cmd)
    subprocess.run(args, cwd=cwd, env=env, check=True)


def cast_calldata(signature: str, *args: str) -> str:
    cmd = ["cast", "calldata", signature, *args]
    result = subprocess.check_output(cmd, text=True).strip()
    return result

def derive_address_from_private_key(private_key: str) -> str:
    cmd = ["cast", "wallet", "address", private_key]
    result = subprocess.check_output(cmd, text=True).strip()
    return result


def forge_script(script_path: str, args: str, inputs: DeploymentInputs, description: str) -> None:
    command = f"forge script {script_path} --legacy"
    extra_args = args.strip()
    if extra_args:
        command = f"{command} {extra_args}"
    run_command(description, command, inputs.l1_contracts_dir, inputs.base_env())

def read_from_file(file_path: str, key: str) -> Optional[str]:
    try:
        with open(file_path, 'r') as file:
            for line in file:
                if line.startswith(f"{key} = "):
                    return line.split(' = ', 1)[1].strip().strip("\"")
    except FileNotFoundError:
        pass
    return None

def _format_toml_value(value: object) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, str):
        escaped = value.replace('"', '\\"')
        return f'"{escaped}"'
    raise TypeError(f"Unsupported TOML value type: {type(value).__name__}")


def update_toml_key(file_path: str, key: str, value: object) -> None:
    serialized_value = _format_toml_value(value)
    lines = []
    found = False
    try:
        with open(file_path, 'r') as file:
            for line in file:
                if line.startswith(f"{key} = "):
                    lines.append(f"{key} = {serialized_value}\n")
                    found = True
                else:
                    lines.append(line)
        if not found:
            lines.append(f"{key} = {serialized_value}\n")
        with open(file_path, 'w') as file:
            file.writelines(lines)
    except FileNotFoundError:
        with open(file_path, 'w') as file:
            file.write(f"{key} = {serialized_value}\n")
