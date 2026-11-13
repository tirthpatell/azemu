FROM python:3.12-slim
RUN pip install --no-cache-dir azure-core==1.41.0 azure-storage-blob==12.31.1 azure-storage-queue==12.18.1 azure-keyvault-secrets==4.12.0
COPY check.py /check.py
CMD ["python", "/check.py"]
