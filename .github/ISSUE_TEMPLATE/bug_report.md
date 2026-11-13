---
name: Bug report
about: azemu answers differently from Azure, or does something it should not
title: ""
labels: bug
---

**azemu version** (the output of `azemu --version`; for the container image: `docker run --rm <the image you use> --version`, or the image tag):

```
```

**How you run it** (binary or container, the options and `AZEMU_*` settings you pass, your operating system):

**Service, SDK and its version** (for example "Blob, `azure-storage-blob` 12.31.1, Python 3.13", or "Service Bus, `Azure.Messaging.ServiceBus` 7.20, .NET 10"):

**What Azure does** (the answer of the real service, a link to its documentation, or the SDK's own expectation. If you have not seen Azure's answer, say so):

**What azemu did** (the request, the answer or the error, and the emulator's log lines around it; run with `--log-level debug` if you can):

**Steps to reproduce** (a few lines of code or `curl` commands that start from a fresh emulator):

**The output of `GET /_azemu/unsupported`** (run `curl http://127.0.0.1:4577/_azemu/unsupported` after the failing request; it lists what azemu knows it does not do):

```
```

Please do not paste keys, connection strings or tokens of real Azure resources. azemu has no sign-in, so a report never needs them.
