"""Run build tools without Windows error dialogs or console windows.

Usage: python scripts/quiet-run.py cargo check --all-targets --locked --offline
This wrapper never starts FeatherDock. Use build.ps1 -Stage to keep the running
application and the canonical executable untouched during a build.
"""
import ctypes
import os
from pathlib import Path
import subprocess
import sys


def main():
    if len(sys.argv) < 2:
        raise SystemExit("Pass a build command, for example: cargo check --locked")
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    options = {}
    if os.name == "nt":
        # Inherited by cargo, rustc, the linker and build scripts. Loader failure
        # must return an exit code instead of opening a modal application error.
        ctypes.windll.kernel32.SetErrorMode(0x0001 | 0x0002 | 0x8000)
        startup = subprocess.STARTUPINFO()
        startup.dwFlags |= subprocess.STARTF_USESHOWWINDOW
        startup.wShowWindow = 0
        options.update(
            startupinfo=startup,
            creationflags=(
                subprocess.CREATE_NO_WINDOW | subprocess.BELOW_NORMAL_PRIORITY_CLASS
            ),
        )
    environment = os.environ.copy()
    environment["CARGO_BUILD_JOBS"] = "1"
    environment["RUST_TEST_THREADS"] = "1"
    result = subprocess.run(
        sys.argv[1:],
        env=environment,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        **options,
    )
    output = result.stdout.decode("utf-8", errors="replace")
    directory = Path(__file__).resolve().parent.parent / "outputs"
    directory.mkdir(exist_ok=True)
    (directory / "last-background-verification.log").write_text(output, encoding="utf-8")
    print(output if result.returncode or len(output) < 8000 else output[-4500:])
    print(f"Build tool exit: {result.returncode}")
    return int(result.returncode != 0)


if __name__ == "__main__":
    raise SystemExit(main())
