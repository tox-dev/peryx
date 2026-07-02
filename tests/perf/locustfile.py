"""Locust scenario for the request workload: simple-page fetches, the request an index serves most.

`BENCH_SIMPLE_URL` carries the absolute simple-index base of the server under test and
`BENCH_PACKAGES` the comma-separated project names to rotate through.
"""

import itertools
import os
from typing import Final

from locust import FastHttpUser, constant, task

__all__ = ["SimpleIndexUser"]

_SIMPLE_URL: Final = os.environ["BENCH_SIMPLE_URL"]
_PACKAGES: Final = itertools.cycle(os.environ["BENCH_PACKAGES"].split(","))
_ACCEPT: Final = "application/vnd.pypi.simple.v1+json, text/html;q=0.1"


class SimpleIndexUser(FastHttpUser):
    """One resolver hammering project pages."""

    host = _SIMPLE_URL
    wait_time = constant(0)

    @task
    def simple_page(self) -> None:
        self.client.get(f"{_SIMPLE_URL}{next(_PACKAGES)}/", headers={"accept": _ACCEPT}, name="simple page")
