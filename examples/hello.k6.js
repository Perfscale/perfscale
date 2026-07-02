// Run with: perfscale run --k6 examples/hello.k6.js
import http from 'k6/http';
import { check, sleep } from 'k6';

export const options = {
  vus: 5,
  duration: '30s',
};

export default function () {
  const res = http.get('https://httpbin.org/get');
  check(res, { 'status is 200': (r) => r.status === 200 });
  sleep(0.2);
}
