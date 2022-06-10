import re
from datetime import datetime
from datetime import timedelta
from typing import Dict
from typing import List

import gitlab.base
import pytz
from util.print import eprint

from .group import Group


class Ci:
    _MONITORED_JOB_TYPES = set(
        [
            "rosetta-hourly",
            "system-tests-hourly",
            "system-tests-nightly",
            "wasm-generator-hourly",
            "wasm-generator-nightly",
        ]
    )

    def __init__(self, url: str, project: str, token: str):
        self.gl = gitlab.Gitlab(url=url, private_token=token)
        self.project = self.gl.projects.get(project)

    # Example:        2022-02-22T15:47:51.081Z
    _ES_TIMESTAMP_FMT = "%Y-%m-%dT%H:%M:%S.%f%z"

    @staticmethod
    def job_ts(job) -> datetime:
        """Returns the timestamp of a GitLab job"""
        return datetime.strptime(job.created_at, Ci._ES_TIMESTAMP_FMT)

    @staticmethod
    def job_url(job) -> str:
        return job._attrs["web_url"]

    def get_last_hourly_jobs(self, page_size=1_000) -> List[gitlab.base.RESTObject]:
        now = datetime.now(pytz.utc)
        jobs: List[gitlab.base.RESTObject]
        jobs = []
        page = 1
        while True:
            eprint(f"Scanning page #{page} ...")

            new_jobs = self.project.jobs.list(
                per_page=page_size,
                page=page,
                as_list=True,
                order_by="id",
                sort="desc",
                include_retried=True,
                # we are potentially interested in all completed jobs
                scope=["success", "failed"],
            )

            for job in new_jobs:
                if job.name in Ci._MONITORED_JOB_TYPES and "ic-prod-tests" in job.tag_list:
                    eprint(f"Job {job.name} was created at {job.created_at}")
                    jobs.append(job)

                # Collect logs for CI pipelines created within the last 1 hour
                # + 5 min (to be sure we don't miss anything)
                if now - Ci.job_ts(job) > timedelta(hours=1, minutes=5):
                    eprint("Processed all CI jobs within the last hour")
                    return jobs

            page += 1
            if not new_jobs:
                eprint("Processed all existing CI jobs")
                return jobs

    def get_hourly_group_names(self) -> Dict[str, Group]:
        """Returns: Map from group names to Groups"""
        jobs = self.get_last_hourly_jobs()
        eprint(f"Found {len(jobs)} jobs")

        groups: Dict[str, Group] = dict()

        for job in jobs:
            eprint(f"Searching for group names for job `{job.name}` ...")
            trace = str(job.trace())
            group_names = re.findall("creating group \\\\\\'(.*?)\\\\\\'", trace)
            if not group_names:
                eprint(f"Warning: cannot find test group name for job {Ci.job_url(job)}")
            for gid in group_names:
                eprint(f" + {gid}\n")
                groups[gid] = Group(gid, url=Ci.job_url(job))

        return groups
