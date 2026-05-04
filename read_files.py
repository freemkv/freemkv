import os

def read_md_and_code_files(directory):
    md_contents = []
    code_contents = []

    for root, dirs, files in os.walk(directory):
        for file in files:
            if file.endswith('.md'):
                file_path = os.path.join(root, file)
                with open(file_path, 'r', encoding='utf-8') as f:
                    content = f.read()
                    md_contents.append((file_path, content))
            elif file.endswith(('.py', '.js', '.rs')):
                file_path = os.path.join(root, file)
                with open(file_path, 'r', encoding='utf-8') as f:
                    content = f.read()
                    code_contents.append((file_path, content))

    return md_contents, code_contents

if __name__ == '__main__':
    directory_to_read = input("Enter the path to the directory: ")
    md_files, code_files = read_md_and_code_files(directory_to_read)

    print("MD Files:")
    for file_path, content in md_files:
        print(f"Contents of {file_path}:")
        print(content)
        print("-" * 40)

    print("\nCode Files:")
    for file_path, content in code_files:
        print(f"Contents of {file_path}:")
        print(content)
        print("-" * 40)
