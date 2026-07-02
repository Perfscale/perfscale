# Run with: perfscale run --locust examples/hello.locust.py --host https://httpbin.org
from locust import HttpUser, task, between


class HelloUser(HttpUser):
    wait_time = between(0.2, 1)

    @task
    def get_homepage(self):
        self.client.get("/get")
