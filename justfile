check:
    python3 -m unittest discover -s tests -v
    bash -n libexec/ferry-transport.sh libexec/ferry-run.sh
