FROM python:3

ADD requirements.txt /
RUN pip install -r requirements.txt

ADD plot.py /
ENTRYPOINT ["python", "plot.py"]
