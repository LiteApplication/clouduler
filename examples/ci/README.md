# Deploying to a clouduler cluster from CI

Three ways in, in order of how much you should prefer them:

| | how it authenticates | when to use it |
|---|---|---|
| [`github-actions.yml`](github-actions.yml) | api key | building an image and rolling out the stack that runs it |
| [`gitlab-ci.yml`](gitlab-ci.yml) | api key | the same, on GitLab |
| [`deploy.sh`](deploy.sh) | api key | anything else, and what the two above call |

All of them do the same two things: push a stack definition, then wait for the
instances to actually come up. The waiting is the part people leave out, and
it is the only part that tells you the deploy worked.

## The api key

Make a dedicated key for CI rather than reusing the bootstrap one:

```
curl -sk -X POST https://cloud.int.example.com/api/keys \
  -H "Authorization: Bearer $ADMIN_KEY" \
  -H 'content-type: application/json' \
  -d '{"name": "ci"}'
```

The response carries the secret once. Store it as `CLOUDULER_TOKEN` in your CI
secret store. Keys are admin-capable, so give CI its own and delete it from the
ui the day the pipeline is retired.

## The certificate

Until ACME has issued a real certificate the cluster serves a self signed one,
and `curl -k` skips the check entirely. That is fine on a private runner on the
internal network and not fine over the internet. Once a real certificate is
issued, drop the `-k` and let the chain be verified: `CLOUDULER_INSECURE=0`
in the scripts here.

## Rolling out a new image

A service pinned to a moving tag (`:latest`, `:main`) does not restart by
itself. Ask the cluster to re-resolve the digest and restart it:

```
POST /api/instances/<stack>.<service>.<index>/update
```

`deploy.sh` does this for every instance of a service and then waits for them to
come back healthy. That is the normal shape of a CI deploy when the image
changed but the stack file did not.
