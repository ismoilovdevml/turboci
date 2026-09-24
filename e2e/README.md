# End-to-end test against a real GitLab

`gitlab-ci.yml` exercises a TurboCI runner (docker executor) through GitLab
itself: checkout in an image without git, shell state across script lines,
artifacts with `needs`, cache save/restore, a service reached by image name and
alias, masked/file/nested variables, a failing job with `after_script`, and
(when `E2E_CANCEL` is set) cancellation from GitLab.

1. Create a project and a project runner with the tag `turboci`, and install
   the runner with its token:

   ```bash
   curl -sSL https://raw.githubusercontent.com/ismoilovdevml/turboci/main/install.sh \
     | sudo bash -s -- --url https://gitlab.example.com --token glrt-XXXX
   ```

   and give it the settings `test:docker-build` and `test:runner-config` use
   in `/etc/turboci-runner.toml`, then restart it:

   ```toml
   environment = ["E2E_RUNNER_ENV=from-runner"]

   [executor.docker]
   privileged = true
   volumes = ["/cache"]
   shm_size = "1g"

   [[executor.docker.services]]
   name = "docker:27-dind"
   alias = "docker"
   command = ["--host=tcp://0.0.0.0:2375", "--tls=false"]
   ```

2. Add project CI/CD variables `E2E_SECRET` (masked) and `E2E_FILE` (type: file).
   Set `E2E_DOCKER_IMAGE` to use another image with the docker CLI than
   `docker:27`. Set `E2E_PRIVATE_IMAGE` to an image of a private registry
   (not yet on the host) to check registry logins.
3. Push `gitlab-ci.yml` as `.gitlab-ci.yml` together with `VERSION`.
4. Expect every job to pass except `test:failure` (allowed to fail with exit
   code 3 after `after_script` runs). Run a pipeline with `E2E_CANCEL=1` and
   cancel `test:cancel` to check cancellation: it stops within seconds and its
   `after_script` still runs.
