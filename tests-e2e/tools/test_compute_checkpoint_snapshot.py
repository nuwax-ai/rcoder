import copy
from types import SimpleNamespace
import unittest

import compute_checkpoint_snapshot as observer


class OwnershipTests(unittest.TestCase):
    def setUp(self):
        self.project = 'rcoder-cfailure-' + 'a' * 16
        self.volume = self.project + '_compute-data'
        self.args = dict(controller='c' * 64, image='sha256:' + 'd' * 64,
                         run_id='run', case_id='case', volume=self.volume)
        self.row = {'Id': self.args['controller'], 'Image': self.args['image'],
                    'State': {'Running': False, 'Status': 'exited'},
                    'Config': {'Labels': {'com.docker.compose.project': self.project,
                        'com.docker.compose.service': 'rcoder', 'rcoder.e2e.run': 'run',
                        'rcoder.e2e.case': 'case'}},
                    'Mounts': [{'Type': 'volume', 'Name': self.volume, 'Destination': '/app/data'}]}

    def test_stopped_exact_controller(self):
        self.assertEqual(observer.inspect_controller(self.row, **self.args)['project'], self.project)

    def test_live_replaced_or_foreign_resources_rejected(self):
        mutations = [lambda r: r['State'].update(Running=True),
                     lambda r: r['State'].update(Status='paused'),
                     lambda r: r.update(Id='another'),
                     lambda r: r.update(Image='another'),
                     lambda r: r['Config']['Labels'].update({'rcoder.e2e.run': 'other'}),
                     lambda r: r['Mounts'][0].update(Name='other'),
                     lambda r: r['Mounts'].append(copy.deepcopy(r['Mounts'][0]))]
        for mutation in mutations:
            row = copy.deepcopy(self.row)
            mutation(row)
            with self.subTest(row=row), self.assertRaises(ValueError):
                observer.inspect_controller(row, **self.args)

    def test_operation_lifecycle_and_identity_must_match(self):
        args = SimpleNamespace(app_id='app', lifecycle_id='L2', operation_id='op')
        record = vars(args).copy()
        self.assertEqual(observer.validate_record(record, args), record)
        for field in record:
            with self.subTest(field=field), self.assertRaises(ValueError):
                observer.validate_record({**record, field: 'foreign'}, args)


if __name__ == '__main__':
    unittest.main()
