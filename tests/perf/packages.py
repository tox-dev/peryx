"""The install workload: a snapshot of PyPI's most-downloaded packages that resolves as one set.

The list follows hugovk's top-pypi-packages ranking (June 2026 snapshot), trimmed of entries that
cannot co-resolve in one environment (pinned dependents of each other are kept; true conflicts and
Python-version-gated names are dropped) so one `uv pip install` covers the whole set.
"""

from typing import Final

__all__ = ["TOP_PACKAGES"]

TOP_PACKAGES: Final = (
    "boto3",
    "urllib3",
    "botocore",
    "requests",
    "certifi",
    "idna",
    "charset-normalizer",
    "typing-extensions",
    "python-dateutil",
    "s3transfer",
    "six",
    "packaging",
    "pyyaml",
    "numpy",
    "setuptools",
    "fsspec",
    "wheel",
    "cryptography",
    "jmespath",
    "cffi",
    "pandas",
    "attrs",
    "click",
    "pycparser",
    "protobuf",
    "jinja2",
    "markupsafe",
    "rsa",
    "pytz",
    "colorama",
    "pyasn1",
    "googleapis-common-protos",
    "importlib-metadata",
    "zipp",
    "pydantic",
    "pyjwt",
    "requests-oauthlib",
    "oauthlib",
    "cachetools",
    "google-auth",
    "pyparsing",
    "tzdata",
    "platformdirs",
    "filelock",
    "virtualenv",
    "tomli",
    "grpcio",
    "sqlalchemy",
    "greenlet",
    "requests-toolbelt",
    "torch",
)
