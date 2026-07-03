//! The workloads' package sets.

/// The top of `PyPI`'s most-downloaded list, snapshotted so runs stay comparable over time; torch is
/// included for one large wheel.
pub const TOP_PACKAGES: &[&str] = &[
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
];

/// The stress wheel's project: the largest of the top packages.
pub const STRESS_PROJECT: &str = "torch";

/// A heavy single-wheel install a CI fleet grabs over and over.
pub const FLEET_PACKAGE: &str = "polars";
