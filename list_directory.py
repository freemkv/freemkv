import os

def list_directory_contents():
    for item in os.listdir('.'):
        print(item)

if __name__ == '__main__':
    list_directory_contents()
