"""Optional per-test deadline; timeouts are errors, never skips."""

import os
import signal
import time
import unittest

from django.test.runner import DiscoverRunner


class TestDeadlineExceeded(BaseException):
    """Escape application-level exception handlers and reach unittest's outcome."""


class DeadlineResultMixin:
    def startTest(self, test):
        super().startTest(test)
        self._previous_handler = signal.getsignal(signal.SIGALRM)
        self._previous_timer = signal.getitimer(signal.ITIMER_REAL)
        self._started = time.monotonic()
        seconds = float(os.environ["MPEDB_DJANGO_TEST_TIMEOUT"])

        def deadline(signum, frame):
            raise TestDeadlineExceeded(f"{test.id()} exceeded {seconds:g} seconds")

        signal.signal(signal.SIGALRM, deadline)
        signal.setitimer(signal.ITIMER_REAL, seconds)

    def stopTest(self, test):
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, self._previous_handler)
        remaining, interval = self._previous_timer
        if remaining:
            remaining = max(0.000001, remaining - (time.monotonic() - self._started))
        signal.setitimer(signal.ITIMER_REAL, remaining, interval)
        super().stopTest(test)


class TimedDiscoverRunner(DiscoverRunner):
    def get_resultclass(self):
        base = super().get_resultclass() or unittest.TextTestResult
        return type("DeadlineResult", (DeadlineResultMixin, base), {})
