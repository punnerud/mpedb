from django.db import models
class Author(models.Model):
    name = models.CharField(max_length=100)
    born = models.IntegerField(null=True)
    def __str__(self): return self.name
class Book(models.Model):
    title = models.CharField(max_length=200)
    author = models.ForeignKey(Author, on_delete=models.CASCADE, related_name="books")
    price = models.FloatField(default=0.0)
    created = models.DateTimeField(auto_now_add=True)
