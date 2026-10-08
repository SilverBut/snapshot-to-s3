import json
import unittest

from ci_gate import REQUIRED_JOBS, failures


class GateTests(unittest.TestCase):
    def jobs(self):
        return {name: {"result": "success"} for name in REQUIRED_JOBS}

    def test_all_success(self):
        self.assertEqual(failures(json.dumps(self.jobs())), [])

    def test_non_success_is_not_a_pass(self):
        for name in REQUIRED_JOBS:
            for result in ("failure", "cancelled", "skipped", "unknown", None):
                with self.subTest(job=name, result=result):
                    jobs = self.jobs()
                    jobs[name]["result"] = result
                    self.assertEqual(len(failures(json.dumps(jobs))), 1)

    def test_missing_or_extra_job_is_rejected(self):
        for name in REQUIRED_JOBS:
            jobs = self.jobs()
            del jobs[name]
            with self.assertRaises(ValueError):
                failures(json.dumps(jobs))
        jobs = self.jobs()
        jobs["unexpected"] = {"result": "success"}
        with self.assertRaises(ValueError):
            failures(json.dumps(jobs))

    def test_malformed_result_is_rejected(self):
        for raw in ("not JSON", "[]", "null", '"success"'):
            with self.subTest(raw=raw), self.assertRaises(ValueError):
                failures(raw)
        jobs = self.jobs()
        jobs["e2e"] = {}
        self.assertEqual(failures(json.dumps(jobs)), ["e2e: missing result"])
        for invalid in (None, "success", [], 1):
            jobs["e2e"] = invalid
            with self.subTest(result=invalid), self.assertRaises(ValueError):
                failures(json.dumps(jobs))


if __name__ == "__main__":
    unittest.main()
