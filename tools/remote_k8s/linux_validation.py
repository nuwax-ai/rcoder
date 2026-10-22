"""Run the exact Linux workspace regression; zero matched tests is a failure."""
import argparse
import os
import re
import subprocess

CASE = ('handler::pod_handler::retirement_tests::'
        'restart_historical_uid_does_not_block_current_owned_compute_recovery')


def validate_result(returncode, output):
    if returncode != 0:
        raise RuntimeError(f'Linux workspace validation exited {returncode}')
    summaries = re.findall(r'^test result: .*$', output, re.MULTILINE)
    if len(summaries) != 1 or not re.fullmatch(
            r'test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured; '
            r'\d+ filtered out; finished in [0-9.]+s', summaries[0]):
        raise RuntimeError('Expected exactly one executed passing test; '
                           'zero matched, skipped, or additional tests are not accepted')
    # With --nocapture a test may print between its name and libtest's `ok`.
    # The exact filter plus the one-success summary still identifies that case.
    if not re.search(r'^test ' + re.escape(CASE) + r' \.\.\.',
                     output, re.MULTILINE):
        raise RuntimeError('The required Linux workspace case did not pass')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--jobs', type=int, default=2)
    args = parser.parse_args()
    if args.jobs < 1:
        parser.error('--jobs must be positive')
    result = subprocess.run(
        ['cargo', 'test', '--locked', '--jobs', str(args.jobs), '-p', 'http-server',
         '--features', 'rcoder-pg', '--lib', CASE, '--', '--exact', '--nocapture',
         '--test-threads=1'],
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
        encoding='utf-8', env={**os.environ, 'CARGO_TERM_COLOR': 'never'}, check=False,
    )
    print(result.stdout, end='', flush=True)
    validate_result(result.returncode, result.stdout)
    print('Linux workspace regression: exactly 1 passed; 0 failed', flush=True)


if __name__ == '__main__':
    main()
