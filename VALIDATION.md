# Validation report

Generated: 2026-08-26T12:28:28Z

## Environment

```text
Linux 145a81e29d43 6.18.35 #1 SMP Fri Aug 21 00:36:21 UTC 2026 x86_64 GNU/Linux
rustc: unavailable
cargo: unavailable
python3: Python 3.13.5
gcc: gcc (Debian 14.2.0-19) 14.2.0
clang: clang version 17.0.0 (https://github.com/swiftlang/llvm-project.git 10999b6d034fe318f3d56c83bddb6572593a8bb0)
git: git version 2.47.3
```

## Static inventory checks

- PASS: `Cargo.toml` exists
- PASS: `README.md` exists
- PASS: `docs/SPEC.md` exists
- PASS: start command is represented
- PASS: attach command is represented
- PASS: capture command is represented
- PASS: rename command is represented
- PASS: doctor command is represented
- PASS: PTY creation is represented
- PASS: controlling-terminal setup is represented
- PASS: cgroup-v2 limit handling is represented
- PASS: durability flush is represented
- PASS: atomic replacement is represented

## Rust checks

- NOT RUN: Cargo is unavailable in this sandbox.

## Python checks

### Python bytecode compilation

```text
exit_code=0
```

### Python tests

```text

==================================== ERRORS ====================================
_________________ ERROR collecting python/tests/test_models.py _________________
ImportError while importing test module '/mnt/data/aplexer-implementation/python/tests/test_models.py'.
Hint: make sure your test modules/packages have valid Python names.
Traceback:
/usr/lib/python3.13/importlib/__init__.py:88: in import_module
    return _bootstrap._gcd_import(name[level:], package, level)
           ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
python/tests/test_models.py:1: in <module>
    from aplexer.models import Session
E   ModuleNotFoundError: No module named 'aplexer'
________________ ERROR collecting python/tests/test_protocol.py ________________
ImportError while importing test module '/mnt/data/aplexer-implementation/python/tests/test_protocol.py'.
Hint: make sure your test modules/packages have valid Python names.
Traceback:
/usr/lib/python3.13/importlib/__init__.py:88: in import_module
    return _bootstrap._gcd_import(name[level:], package, level)
           ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
python/tests/test_protocol.py:3: in <module>
    from aplexer.protocol import DATA, recv_frame, send_frame
E   ModuleNotFoundError: No module named 'aplexer'
=========================== short test summary info ============================
ERROR python/tests/test_models.py
ERROR python/tests/test_protocol.py
!!!!!!!!!!!!!!!!!!! Interrupted: 2 errors during collection !!!!!!!!!!!!!!!!!!!!
2 errors in 0.09s
exit_code=2
```


## Result

**One or more checks failed or could not be completed. See details above.**
