import copy
import json
import unittest

from compare import compare, measures


def fixture():
    return {
        "run": 1,
        "config": {"resolution": "1920x1080", "fps": 60, "codec": "h264", "workload": "cube"},
        "environment": {"cpu": "test"},
        "counters": {"capture_attempts": 600, "encode_backpressure": 0, "send_failed_frames": 0},
        "summary": {
            "stages_us": {stage: {"p50": 1000, "p95": 1500, "p99": 2000}
                          for stage in ("host_total", "network_queue", "loopback_complete")},
            "successful_fps": 60, "successful_frames": 600, "over_frame_budget": 0,
            "incomplete_receipts": 0,
        },
    }


class CompareTests(unittest.TestCase):
    def test_faster_latency_with_more_drops_is_flagged(self):
        a = fixture()
        b = copy.deepcopy(a)
        b["summary"]["stages_us"]["host_total"]["p99"] = 1000
        b["counters"]["encode_backpressure"] = 60
        text, regression = compare({json.dumps(a['config']): [a]}, {json.dumps(b['config']): [b]}, fail_percent=5)
        self.assertIn("Lower successful-frame latency alone", text)
        self.assertTrue(regression)

    def test_environment_mismatch_requires_explicit_override(self):
        a, b = fixture(), fixture()
        b["environment"]["cpu"] = "different"
        with self.assertRaises(ValueError):
            compare({json.dumps(a['config']): [a]}, {json.dumps(b['config']): [b]})

    def test_missing_tail_is_not_treated_as_zero(self):
        a = fixture()
        a["summary"]["stages_us"]["host_total"]["p99"] = None
        with self.assertRaises(ValueError):
            measures(a)

    def test_configuration_mismatch_is_rejected(self):
        with self.assertRaises(ValueError):
            compare({'a': [fixture()]}, {'b': [fixture()]})

    def test_buffer_age_regression_is_visible_even_if_processing_is_faster(self):
        a, b = fixture(), fixture()
        for doc, value in ((a, 500), (b, 10000)):
            doc["summary"]["stages_us"]["buffer_age"] = {"p50": value, "p99": value}
        b["summary"]["stages_us"]["host_total"]["p99"] = 1000
        text, regression = compare({json.dumps(a['config']): [a]}, {json.dumps(b['config']): [b]}, fail_percent=5)
        self.assertIn("buffer_age p99 us", text)
        self.assertTrue(regression)

    def test_old_reports_remain_comparable_with_explicit_missing_metric_notice(self):
        a, b = fixture(), fixture()
        b["summary"]["stages_us"]["buffer_age"] = {"p50": 500, "p99": 1000}
        text, _ = compare({json.dumps(a['config']): [a]}, {json.dumps(b['config']): [b]})
        self.assertIn("optional timing metrics absent", text)
